# Security Policy

## Scope

The `content-hub-service` source and image (`content-hub` binary: `admin`, `public`, `worker`, `migrate`, `seed`,
`export-snapshot`). Out of scope: Cloudflare Access, the identity provider (Keto), the platforms posted to, the
database tier and the cluster, and the client and UI packages (see their own policies).

## Security model

1. **Separate processes, separate privileges.** The public process connects as a PostgreSQL role that can read
   only the public views (no table of schema `hub`, no write, no temporary tables); internal fields cannot reach a
   public response even through a bug. Only the `migrate` Job can change the schema. The admin process's writer role
   has no DDL and cannot truncate; the audit log and spend ledger are append-only.
2. **Admin access is checked twice.** Cloudflare Access gates the admin host; the backend verifies the Access JWT
   (signature, audience, issuer, expiry) on every request and requires a Keto role; state-changing requests also need a
   CSRF header and a same-origin `Origin`. Keto being unreachable fails closed (503); the development verifier (a
   static JWKS and admin list) is refused unless the process is explicitly started as `development`.
3. **At-most-once publishing.** A send is marked `sending` and committed before the external call; an uncertain send
   becomes `unknown` and is never retried automatically; the database refuses a second live instance of a post on the
   same target. Nothing is sent unless the worker runs with `PUBLISHING_LIVE=1` and the target is marked live
   (outside production only sandbox targets); otherwise every send is a recorded dry run.
4. **Secrets.** Platform tokens and webhook URLs live only in Kubernetes Secrets and are referenced by
   environment-variable name, which must start with `PLATFORM_`; the database refuses a value or any other name in
   that field. Nothing logs, stores or returns a credential: recorded requests are redacted, and reply excerpts and
   error reasons are scrubbed of it.
   The database connection is TLS `verify-full` only; its URL and password are never logged or put in an error.
5. **Bounded spend.** Every paid path is capped (default 0) and metered before the action: enqueue reserves the cost under every applicable cap or
   hands the post to a person instead; the worker settles it when the post went out and releases it when it did not.
6. **Validated input.** Every URL is `https` and refused when it names a private, loopback or link-local address;
   the worker's client resolves names itself, connects only to public addresses it checked, follows no redirects and
   ignores proxies; request bodies are size-limited (1 MiB admin, 16 KiB public) and time-limited; payloads are rendered and counted
   per platform before they can be queued.
7. **A snapshot signer that holds nothing.** The roadmap snapshot is signed by a key that owns no funds, object or
   capability; storage is paid by the Walrus publisher's own wallet (a capped float) behind short-lived tokens bound to the
   exact blob. The hub's client for the publisher calls only its configured host; mainnet exports need an explicit switch.
   A leaked signer can forge snapshots (rotate and publish a notice); it cannot move money.
8. **Least privilege at run time.** Distroless image, nonroot (65532), no shell, read-only root filesystem, no
   capabilities; Rust with no `unsafe`. The namespace denies ingress and egress by default and allows only the database,
   Keto, the Access JWKS host, the Walrus publisher and the platform API hosts.

## Supported versions

The latest release. Pre-0.2 versions receive fixes only in the next release.

## Reporting a vulnerability

Please **do not** open a public issue. Email **<security@meddleware.co.uk>** with a description, reproduction
steps, and the version or commit. Acknowledgement within **3 business days**; a plan within **14 days** for confirmed
issues.

## Disclosure

Coordinated: a fix is released first, then the issue is described in the CHANGELOG and the audit
(`docs/audit/content-hub-service-audit.md`). Reporters are credited unless they ask otherwise.
