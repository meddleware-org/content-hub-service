//! The route tables: the contract of each process's HTTP surface (ARCHITECTURE.md §3, route exposure).
//!
//! Each router is built from its table and a test proves the router answers exactly these routes, so
//! the table, the router and the documentation cannot drift apart.

/// The HTTP methods the tables use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    /// Whether the method changes state (and so needs the CSRF check on the admin surface).
    pub const fn is_state_changing(self) -> bool {
        !matches!(self, Method::Get)
    }
}

/// One route a process serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub method: Method,
    pub path: &'static str,
    /// What it is for (rendered into the docs table).
    pub purpose: &'static str,
}

const fn r(method: Method, path: &'static str, purpose: &'static str) -> Route {
    Route { method, path, purpose }
}

use Method::{Delete, Get, Patch, Post, Put};

/// Served by both processes, on their own ports and hosts.
pub const PROBE_ROUTES: &[Route] = &[
    r(Get, "/healthz", "liveness: the process runs (no database call)"),
    r(Get, "/readyz", "readiness: the database is reachable and migrated to this build's schema"),
];

/// The `public` process (hosts roadmap. and feeds.). Read-only role; no admin route exists in it.
pub const PUBLIC_ROUTES: &[Route] = &[
    r(Get, "/public/v1/roadmap", "public roadmap for people (visible_human)"),
    r(Get, "/public/v1/roadmap-bot", "public roadmap for automated clients (visible_bot), stable versioned JSON"),
    r(Get, "/public/v1/snapshot", "the latest signed roadmap snapshot: hash, signature, signer, Walrus blob"),
    r(Get, "/public/v1/feed.json", "JSON Feed 1.1 of the owned feed"),
    r(Get, "/public/v1/feed.xml", "RSS 2.0 of the owned feed"),
    r(Get, "/public/v1/openapi.json", "OpenAPI description of the public routes"),
    r(Get, "/llms.txt", "description of the machine host for agents"),
    r(Get, "/robots.txt", "crawler policy of the machine host"),
];

/// The `admin` process (host hub., behind Cloudflare Access). Writer role.
pub const ADMIN_ROUTES: &[Route] = &[
    r(Get, "/api/v1/me", "the signed-in operator's display name"),
    r(Get, "/api/v1/openapi.json", "OpenAPI description of the admin routes"),
    r(Get, "/api/v1/phases", "list roadmap phases"),
    r(Post, "/api/v1/phases", "create a phase"),
    r(Get, "/api/v1/phases/{id}", "read a phase"),
    r(Patch, "/api/v1/phases/{id}", "update a phase"),
    r(Get, "/api/v1/items", "list roadmap items (filters, cursor)"),
    r(Post, "/api/v1/items", "create an item"),
    r(Get, "/api/v1/items/{id}", "read an item with its tasks and posts"),
    r(Patch, "/api/v1/items/{id}", "update an item"),
    r(Get, "/api/v1/tasks", "list tasks (filters, cursor)"),
    r(Post, "/api/v1/tasks", "create a task"),
    r(Get, "/api/v1/tasks/{id}", "read a task"),
    r(Patch, "/api/v1/tasks/{id}", "update a task's fields"),
    r(Post, "/api/v1/tasks/{id}/transition", "change a task's status (state machine)"),
    r(Post, "/api/v1/tasks/{id}/links", "attach a post, an instance or an external link"),
    r(Delete, "/api/v1/tasks/{id}/links/{link_id}", "detach a link"),
    r(Get, "/api/v1/posts", "list posts (filters, cursor)"),
    r(Post, "/api/v1/posts", "create a post"),
    r(Get, "/api/v1/posts/{id}", "read a post with its revisions"),
    r(Patch, "/api/v1/posts/{id}", "edit a post (new revision)"),
    r(Post, "/api/v1/posts/{id}/enqueue", "validate, reserve spend and queue instances"),
    r(Get, "/api/v1/instances", "list platform instances (filters, cursor)"),
    r(Get, "/api/v1/instances/{id}", "read an instance with its attempts"),
    r(Post, "/api/v1/instances/{id}/transition", "operator moves (mark posted, retry, resolve unknown, cancel)"),
    r(Get, "/api/v1/platforms", "list platform definitions"),
    r(Patch, "/api/v1/platforms/{id}", "edit a definition's limits, templates or enabled flag"),
    r(Get, "/api/v1/targets", "list platform targets"),
    r(Post, "/api/v1/targets", "create a target (secret NAME only)"),
    r(Patch, "/api/v1/targets/{id}", "update a target"),
    r(Get, "/api/v1/budgets", "caps, usage and the instance ceilings"),
    r(Put, "/api/v1/budgets", "set runtime caps (at or below the ceilings)"),
    r(Get, "/api/v1/publishing", "the publishing stop switch"),
    r(Put, "/api/v1/publishing", "stop or resume the worker's sending"),
    r(
        Get,
        "/api/v1/preview/roadmap",
        "the public roadmap exactly as the public process serves it (audience=human|bot)",
    ),
    r(Get, "/api/v1/schedule", "upcoming scheduled instances and reviews"),
    r(Get, "/api/v1/stats", "counts for the dashboard and Grafana"),
    r(Get, "/api/v1/audit", "audit log (cursor)"),
];

/// The `worker` process (no ingress; the cluster scrapes and probes it), next to the probes.
pub const WORKER_ROUTES: &[Route] = &[r(Get, "/metrics", "Prometheus metrics: queue, attempts, spend against caps")];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_disjoint_and_unique() {
        let mut all: Vec<(Method, &str)> = PROBE_ROUTES
            .iter()
            .chain(PUBLIC_ROUTES)
            .chain(ADMIN_ROUTES)
            .chain(WORKER_ROUTES)
            .map(|r| (r.method, r.path))
            .collect();
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n, "a route appears twice");
    }

    #[test]
    fn prefixes_separate_the_processes() {
        assert!(
            PUBLIC_ROUTES
                .iter()
                .all(|r| r.path.starts_with("/public/v1/") || matches!(r.path, "/llms.txt" | "/robots.txt"))
        );
        assert!(ADMIN_ROUTES.iter().all(|r| r.path.starts_with("/api/v1/")));
        assert!(PUBLIC_ROUTES.iter().all(|r| r.method == Method::Get), "the public process is read-only");
    }
}
