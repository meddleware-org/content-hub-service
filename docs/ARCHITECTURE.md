# Content hub — architecture

**Status:** accepted design, version 1 (2026-10-25). The schema, the migration runner, the route tables, the state
machines and the generic platform definitions are implemented; the handlers, the worker, the seed import and the
snapshot export follow this document (each section says what exists).

The content hub is the single source of truth for an operator's announcements and public roadmap. Every channel is
downstream of it: a post is written once, rendered per platform, and either sent automatically (API, webhook, the
hub's own feeds) or posted by a person and recorded as done. Nothing about an operator's own instance (host names,
channels, wording, plans) is in this repository; it lives in the operator's private deployment repository.

Hosts below use the operator's two zones as placeholders: the **human zone** (sites for people) and the **machine
zone** (endpoints for automated clients), for example `hub.<human zone>` and `feeds.<machine zone>`.

---

## 1. Context

| Part | Repository | What it is |
| --- | --- | --- |
| Backend | `content-hub-service` (this) | One Rust binary, six subcommands (§3); PostgreSQL |
| Client | `content-hub-client` | npm package: route contract, enumerations, state machines, the golden vectors |
| Admin UI | `content-hub-ui` | Vue 3 single-page app on `hub.`; exports `ContentHubView` |
| Public site | `roadmap-site` | Vue 3 site on `roadmap.`; reads `/public/v1/roadmap` |

A **post** is canonical Markdown. A **platform definition** describes a platform (limits, counting, markup, intent
links, cost). A **target** is one destination on a platform (a channel, a subreddit). A **platform instance** is one
post rendered for one target, with its own lifecycle — an automated send or a manual post are both instances, so
every publication is tracked the same way. **Roadmap phases, items and tasks** plan the work; each phase and item
has internal wording and separate public wording, and two visibility flags (`visible_human`, `visible_bot`).

## 2. Decisions

| Decision | Choice | Reason |
| --- | --- | --- |
| Language | Rust (edition 2024), tokio | Fleet rule: no JavaScript/TypeScript backends |
| HTTP | axum 0.8 + tower | Routing and extractors for about 45 routes; tower layers for limits and timeouts |
| Database access | SQLx 0.9 (Postgres only), compile-time-checked queries with offline metadata in `.sqlx/`, rustls on ring, `verify-full` only | A CRUD schema of 18 tables with many queries: the compiler checks every query against the migrated schema. sui-indexer uses tokio-postgres instead because it has a handful of hand-tuned queries with plan tests and no CRUD; both use rustls with ring (one TLS stack). SQLx 0.9 also refuses dynamic SQL unless wrapped in `AssertSqlSafe`, which makes every dynamic statement visible in review |
| OpenAPI | utoipa 6: schemas from the request and response types; paths built from the route tables (`routes.rs`) | One route list drives the routers, the documents, the client's tables and the contract job; a test fails if a route is undocumented. Served at both prefixes |
| Outbound HTTP (JWKS, Keto) | reqwest 0.13 with `rustls-no-provider`; the binary installs ring as the process-wide provider | Keeps one TLS and crypto stack (ring); no aws-lc-rs |
| Seed format | YAML (serde-saphyr) or JSON, validated by `content_hub_core::seed` before anything is written | The operator writes YAML by hand; serde-saphyr is maintained and has no `unsafe-libyaml` dependency |
| Time | chrono | SQLx, utoipa and the feeds share one date type |
| Admin authentication | Cloudflare Access in front; the backend verifies the Access JWT itself behind an `IdentityVerifier` port; roles from Keto (§4) | The platform has no login UI of its own yet; Access is the fleet's admin gate. The port lets a self-hosted OIDC login replace Access without touching handlers |
| JWT verification | RS256 checked with ring's RSA verifier against the team JWKS | `jsonwebtoken` 11 offers only aws-lc-rs (a second crypto stack) or RustCrypto `rsa` (advisory RUSTSEC-2023-0071 fails cargo-deny) |
| Process split | `admin`, `public`, `worker` Deployments; `migrate`, `seed` Jobs; `export-snapshot` CronJob — one binary | Least privilege per process (§3, §6); one image to build, sign and pin |
| Database roles | owner (`migrate`), writer (`admin`, `worker`, `seed`, `export-snapshot`), read-only (`public`) | The writer has DML on schema `hub` and no DDL; the read-only role can read only the public views. The `writer` record type is a change to the platform's database tier (§6 "Roles") |
| Migrations | Embedded (`sqlx::migrate!`), forward-only, run by the `migrate` Job before each rollout, each compatible with the previous release | Application roles cannot run DDL; old pods keep working while the Job runs; a schema from a newer build is refused |
| Queue | `platform_instances` rows claimed with `FOR UPDATE SKIP LOCKED`; no separate outbox, no Dragonfly | The instance row is already the unit of work and its state machine; a second queue would duplicate state that must stay consistent. Enqueue is one transaction |
| Frontends | Vue 3 + Vite + `@meddleware/ui`, served by static-server | Fleet pattern; same-origin `/api` and `/public/v1`, no CORS |
| Snapshot | Canonical JSON of both public roadmaps, signed with a Sui personal-message signature (Ed25519, BLAKE2b-256; checked against `@mysten/sui` in CI), stored on Walrus through an internal publisher (`BlobStore` port; HS256 tokens bound to the exact epochs and size). The signer holds nothing spendable; mainnet is off unless explicitly allowed. [docs/SNAPSHOT.md](SNAPSHOT.md) | Anyone can verify it without trusting the operator's servers; no TypeScript SDK in a backend; a leaked signer forges snapshots, never moves funds (ADR-0007) |
| Media | Not in version 1; later presigned URLs on S3-compatible storage | No uploads means no file-handling surface yet |
| Network | Own namespace, default-deny ingress and egress; egress only to PostgreSQL, Keto (read), the Access JWKS host, the Walrus publisher and the platform API hosts (by FQDN) | A compromised process cannot reach anything else |
| Cost caps | Layered caps, metered before the action (below) | An operator without a budget must not be able to overspend by misconfiguration |
| Licence | 0BSD | Fleet licence; reusable by any operator |
| Publishing code layout | Request bodies and reply classification per platform are pure (`core::platform`); HTTP in `content-hub-publish`; the loop in `content-hub-worker` | Every platform rule is unit-tested without a network; the worker crate is the only one that joins the database to the network |
| Payload snapshot | Enqueue stores the rendered, validated payload (title, body, SHA-256, post revision) on the instance; the worker sends exactly that. A move back into `queued` re-renders and re-validates | A post edited after it was queued cannot reach a platform unvalidated; every attempt records the hash of what it sent |
| Live sends | Three switches: the worker's `PUBLISHING_LIVE=1`, the target's `live`, and production or a sandbox target. Otherwise a dry run (the exact redacted request) and `manual_required`. A stop switch (`publishing.paused`) | Nothing is sent by default; the first live send is a deliberate act per target |
| Outbound HTTP for publishing | One client: https only, no redirects, no proxies, names resolved by the worker and non-public addresses dropped before connecting; 5 s connect, 30 s total, replies read to 64 KiB | The resolved-address SSRF check cannot be raced (the client connects only to what it checked); a timeout after sending is distinguishable from a connection never made |
| Credential names | `secret_ref` must be `PLATFORM_<name>` (database CHECK, API, worker) | A target cannot make the worker read `DATABASE_URL` or another process setting and send it to a host the operator chose |
| X authentication | OAuth 1.0a user context (HMAC-SHA1 via ring); OAuth 2.0 PKCE not implemented | OAuth 2.0 refresh tokens rotate on every use, so the worker would have to store a credential; OAuth 1.0a tokens do not expire. The publisher ships disabled (pay-per-use only) |
| Bluesky record keys | A TID from the first attempt's time and the idempotency key | Every attempt of one instance writes the same record, and the post's URL is known before the call (it is in the attempt record) |

### Cost caps

Every spend path (paid posting APIs, ads, the snapshot wallet) has a cap. Layers, each of which can only **tighten**
the one above:

1. **Code default** — the safe minimum used when nothing is configured: every paid path 0.
2. **Instance configuration** — environment / ConfigMap in the operator's deployment repository; the ceiling.
3. **Runtime setting** — `hub.spend_caps`, edited on the admin UI's Budgets page, at or below the ceiling.
4. **Per-target override** — a `target`-scope row, at or below its platform's cap.

Spend is **metered before the action**: the enqueue transaction writes a `reserve` row to `hub.spend_ledger` and
refuses the instance (→ `manual_required`) if the month's reservations would pass any applicable cap; the worker
`settle`s on success and `release`s on refusal or cancellation. Alerts fire at 50 %, 80 % and 100 % (metrics, and an
optional message to an internal channel). At 100 % nothing more is spent. Months reset at 00:00 UTC on the 1st. Every
change to a cap is audit-logged.

## 3. Topology and route exposure

| Process | Host(s) | Serves | Database role | Replicas |
| --- | --- | --- | --- | --- |
| `admin` | `hub.<human zone>` `/api/` (the UI serves `/`) | admin routes + probes | writer | 2 |
| `public` | `roadmap.<human zone>` (`/public/v1/roadmap` only) and `feeds.<machine zone>` | public routes + probes | read-only | 2 |
| `worker` | — (no ingress) | probes + `GET /metrics` (scraped in-cluster) | writer | 1 (more are safe: `SKIP LOCKED`) |
| `migrate` | Job before each rollout | — | owner | — |
| `seed` | Job on demand | — | writer | — |
| `export-snapshot` | CronJob | — | writer | — |

The two routers take different state types, so a handler written for one cannot be mounted on the other. Each
router is built from its route table (`crates/api/src/routes.rs`); the tests prove that each router answers its
own table and 404s the other's, and the contract job proves the published client's tables match the binary.

| Route | `public` | `admin` | Ingress exposes it on |
| --- | :-: | :-: | --- |
| `GET /healthz`, `GET /readyz` | ✓ | ✓ | nowhere (probes are cluster-internal) |
| `GET /public/v1/roadmap` | ✓ | 404 | `roadmap.` |
| `GET /public/v1/roadmap-bot` | ✓ | 404 | `feeds.` |
| `GET /public/v1/feed.json`, `/public/v1/feed.xml` | ✓ | 404 | `feeds.` |
| `GET /public/v1/snapshot` | ✓ | 404 | `roadmap.` (the verifiable record on the site) and `feeds.` |
| `GET /public/v1/openapi.json` | ✓ | 404 | `feeds.` |
| `GET /llms.txt`, `GET /robots.txt` | ✓ | 404 | `feeds.` (each frontend ships its own for its host) |
| any write method on a public path | 405 | 404 | — |
| `/api/v1/*` (38 routes, `routes.rs`) | 404 | ✓ | `hub.` only, behind Access |
| anything else | 404 | 404 | — |

The ingress for `roadmap.` routes exactly `/public/v1/roadmap` and `/public/v1/snapshot` to `public`; `feeds.` routes only the feed paths. No
ingress routes `/api/` anywhere but `hub.`.

## 4. Admin authentication and authorisation

- **At the edge:** a Cloudflare Access application on `hub.` (deny by default, the operator's identity, MFA), and the
  tunnel's `originRequest.access` check, so a request without a valid Access token never reaches the origin.
- **In the backend, on every `/api` request** (not only trusting the edge): the `Cf-Access-Jwt-Assertion` header is
  verified — RS256 against the team JWKS (`https://<team>.cloudflareaccess.com/cdn-cgi/access/certs`, cached, refreshed
  on an unknown `kid`), `aud` equal to the application's AUD tag, `iss` equal to the team domain, `exp` and `nbf` with a
  small clock-skew bound. Missing or invalid → **401**.
- **Authorisation:** the JWT `sub` must hold `ContentHub:admin#member@<sub>` in Keto (checked through Keto's read
  API, cached briefly). Not a member → **403**. Keto unreachable → **503** (fail closed).
- **CSRF:** every state-changing request needs the custom header `X-Content-Hub-Request` and an `Origin` equal to the
  admin host; otherwise **403**. A cross-site form cannot set the header.
- **Session:** Access holds it; the UI holds no token. `GET /api/v1/me` returns the display name; logout is a link to
  `/cdn-cgi/access/logout`. `hub.users` keeps only the subject, a display name and first/last-seen times.
- **Development:** a dev verifier with a test JWKS behind an explicit environment flag, refused when the production
  configuration (team, AUD) is set.
- **Later:** Access is replaced platform-wide by a self-hosted OIDC login. The hub then registers a confidential
  client (created by the platform's setup Job, secrets passed only through the environment variables the identity
  provider reads); only the `IdentityVerifier` implementation changes.

## 5. API

- **Admin, `/api/v1`:** JSON; OpenAPI at `/api/v1/openapi.json` (authenticated); cursor pagination (`?cursor=&limit=`,
  opaque cursors, limit ≤ 100); filters per resource (status, type, phase, platform, date ranges). Errors are
  `{"error": "<code>", "message": "<text>"}`.
- **Public, `/public/v1`:** read-only; dedicated response types built only from the `public.public_v1_*` views (§6);
  `ETag` and `Cache-Control` on every response; no internal field can appear because the process cannot read one.
  - `/roadmap` — phases and items with `visible_human`;
  - `/roadmap-bot` — the same shape for `visible_bot`, versioned and documented for automated clients;
  - `/feed.json` (JSON Feed 1.1) and `/feed.xml` (RSS 2.0) — posts published to an owned-feed target.
- **Probes:** `/healthz` (no database call) and `/readyz` (database reachable and migrated to this build's schema).

## 6. Data model

PostgreSQL 18. Migration `crates/store/migrations/0001_init.sql` is the schema; this section summarises it.

### Layout and roles

- Every table is in schema **`hub`**. The views the public process reads are in schema **`public`**, together with
  SQLx's `_sqlx_migrations` table (so every role can read the schema version).
- Enumerations are check-constrained `text`; a store test compares every CHECK list with the Rust enum, and the
  golden vectors compare the Rust and TypeScript lists.
- Times are `timestamptz` (UTC); money is integer micro-US-dollars; public identifiers are random `public_id` UUIDs,
  never internal keys (a key can reveal internal planning).

| Role | Connects as | Privileges |
| --- | --- | --- |
| owner | `migrate` Job | owns the database, `hub` and the views; DDL |
| writer | `admin`, `worker`, `seed`, `export-snapshot` | `USAGE` on `hub`; `SELECT, INSERT, UPDATE, DELETE` on its tables (no `TRUNCATE`, no DDL); `SELECT` on `public` |
| read-only | `public` | `USAGE` and `SELECT` on `public` only; read-only transactions; no `TEMPORARY` |

On the platform's database tier this is one `service` with three role records, the `writer` record type being new:

```text
service  content_hub  namespace=content-hub
owner    content_hub  content_hub_migrate  connection_limit=2
writer   content_hub  content_hub_app      connection_limit=12 statement_timeout=30s schema=hub
readonly content_hub  content_hub_public   connection_limit=10 statement_timeout=5s
```

The `writer` record creates the data schema owned by the owner, grants `USAGE` and default DML privileges on it, and
`SELECT` on `public`. Identity columns need no sequence grants (verified by the store tests). On any other
PostgreSQL the same statements apply; `crates/store/src/testdb.rs` is the reference.

### Tables

| Table | Holds | Notable constraints |
| --- | --- | --- |
| `users` | subject, display name, first/last seen | nothing else about a person |
| `roadmap_phases` | key, `public_id`, order, internal and public name/description, window, visibility | unique order (deferrable, so phases can swap) |
| `roadmap_items` | phase, key, `public_id`, type, internal and public title/description, visibility, status, internal dates, an optional public date | a visible item must have public wording; only `public_due_at` is ever public |
| `tasks` | item, key, type, title, status, blocked reason, schedule, completion, review interval, assignee, entity-gated | blocked ⇔ reason; done ⇔ completion time |
| `task_dependencies` | task → task it depends on | no self-dependency (cycles refused by the API) |
| `task_platforms` | task ↔ platform (and optional target) | the target must be on that platform (composite foreign key) |
| `task_links` | task → post, instance or https URL | exactly the column of its kind |
| `platform_definitions` | code, category, posting mode, counting method, limits, markup, base URL, URL templates, cost model, enabled | a limit needs a counting method |
| `platform_targets` | platform, name, external reference, non-secret config, `secret_ref`, sandbox, enabled, live | `secret_ref` is an environment-variable NAME (`^PLATFORM_[A-Z0-9_]{1,55}$`): a URL, a token or another process setting is refused |
| `posts`, `post_revisions` | canonical Markdown, status, tags, categories, links to roadmap/task, revisions | — |
| `platform_instances` | post × target, overrides, status, external status, URL/id, schedule, attempts, idempotency key, cost estimate, metrics, the payload (title, body, SHA-256, post revision) | at most one live instance per post and target; posted ⇒ posted time; sending ⇔ start time; the payload is all or nothing, and queued or sending ⇒ a payload; posts with instances cannot be deleted |
| `delivery_attempts` | each send or dry run, outcome, HTTP status, redacted excerpt and request, payload hash | outcome ⇔ finished; dry run ⇔ outcome `dry_run` |
| `spend_caps` | runtime caps by scope (total, platform, target) | scope matches the columns set |
| `spend_ledger` | reserve / settle / release rows per month | append-only (trigger) |
| `settings` | runtime settings (e.g. `publishing.dry_run`) | dotted lower-case keys |
| `audit_log` | actor, action, entity, before/after | append-only (trigger, also refuses `TRUNCATE`) |
| `snapshots` | network, time, schema version, sha256, size, signature, signer, blob/object id, epochs, expiry | ≤ 1 MiB; stored ⇔ blob id, epochs and expiry |

### Public views

| View | Columns | Rows |
| --- | --- | --- |
| `public_v1_phases` | `public_id, sort_order, name, description, window_start, window_end, visible_human, visible_bot, updated_at` | phases visible to either audience |
| `public_v1_items` | `public_id, phase_public_id, title, description, status, due_at, visible_human, visible_bot, updated_at` (`due_at` is the item's optional `public_due_at`) | items visible to an audience **whose phase is visible to that audience too** |
| `public_v1_feed_entries` | `public_id, title, body_markdown, published_at, updated_at` | posted instances on enabled `feed` targets of non-draft posts |
| `public_v1_snapshots` | every snapshot column except the internal id | all (they are public by design) |

The views are `security_barrier` views owned by the owner; a test pins their exact column lists. Adding a column
is an exposure decision: change the view, the test, the response types and this table together.

### State machines

Tasks (admin API; `crates/core/src/task_state.rs`):

| from \ to | planned | ready | in_progress | done | blocked |
| --- | :-: | :-: | :-: | :-: | :-: |
| planned | — | ✓ | ✓ | | ✓ |
| ready | ✓ | — | ✓ | | ✓ |
| in_progress | | ✓ | — | ✓ | ✓ |
| done | | | ✓ (reopen) | — | |
| blocked | ✓ | ✓ | ✓ | | — |

`blocked` needs a reason; starting or finishing needs every dependency done.

Platform instances (`crates/core/src/instance_state.rs`) — who may make each move:

| from | to | actor |
| --- | --- | --- |
| planned | queued · manual_required · cancelled | operator |
| queued | sending · manual_required | worker |
| queued | planned · cancelled | operator |
| sending | posted · failed · queued (provably not sent) · unknown | worker |
| sending | unknown | janitor |
| failed | queued · manual_required · cancelled | operator |
| unknown | posted · failed | operator |
| manual_required | posted · planned · cancelled | operator |
| cancelled | planned | operator |
| posted | — (terminal) | |

## 7. Seed

`content-hub seed` upserts, idempotently by code and key:

1. the **generic platform definitions** (`seed/platform-definitions.json`, in this repository, embedded in the
   binary; validated by `content_hub_core::platforms`);
2. the **instance seed** given by `--path` (the operator's roadmap: phases, items, tasks; never baked into an image),
   validated against `schema/roadmap-seed.schema.json`.

The 38 generic definitions, checked against each platform's documentation on 2026-10-25 (facts that could not be
confirmed are marked *verify* in the file's notes and are not enforced):

| Code | Category | Mode | Counting / limit | Markup | Enabled |
| --- | --- | --- | --- | --- | --- |
| OWNED_FEED | owned_channel | feed | — | markdown | yes |
| DISCORD_ANNOUNCEMENTS, DISCORD_PER_PACKAGE | social | webhook | code points / 2000 (embed description 4096, embeds 6000) | discord_md | yes |
| DISCORD_SOCIAL_FEED | internal | webhook | code points / 2000 | discord_md | yes |
| TELEGRAM_BROADCAST | social | api | UTF-16 / 4096 | telegram_html | yes |
| BLUESKY | social | api | graphemes / 300, bytes 3000 | plain (+ facets) | yes |
| MASTODON | social | api | graphemes / 500 default, per instance; URL 23 | plain | yes |
| X | social | api | x_weighted / 280; URL 23 | plain | **no** (pay-per-use; intent link meanwhile) |
| FARCASTER | social | api | bytes / 320 (long cast 1024) | plain | **no** (evaluate) |
| REDDIT | forum | manual | code points / 40000, title 300 (*verify*) | markdown | yes |
| BITCOINTALK | forum | manual | — (*verify*) | bbcode | yes |
| DEV_FORUMS | forum | manual | — | markdown | yes |
| YOUTUBE | content | manual | code points / 5000, title 100 | plain | yes |
| BLOG, KOL_CONTENT | content | manual | — | markdown / plain | yes |
| AUDIO_EVENTS | event | manual | — | plain | yes |
| SUI_DIRECTORY, ECOSYSTEM_TRACKERS, DAPPRADAR, DAPP_DIRECTORIES, AIRDROP_DIRECTORIES | directory | manual | — | plain | yes |
| ONCHAIN_BADGES | onchain | manual | — | plain | yes |
| CRYPTO_AD_NETWORKS, WEB3_AD_NETWORKS, BRAVE_ADS | ad_network | manual | — | plain | yes (budget 0) |
| SUI_FOUNDATION, AGENCIES | partnership | manual | — | plain | yes |
| MCP_REGISTRY, MCP_DIRECTORIES, AGENT_DIRECTORIES, HUGGING_FACE, MOVE_REGISTRY, SOURCE_VERIFICATION, AWESOME_LISTS | registry | manual | — | plain / markdown | yes |
| WALRUS_SITES | owned_channel | manual | — | plain | yes |
| DOCS_AUTODOC, ADMIN_BOT, AUTOMATION_FRAMEWORKS | internal | none | — | markdown / plain | yes |

"Enabled" means usable; nothing is sent until a target exists, its secret is configured and dry-run is switched off
for it. Intent links: X `x.com/intent/tweet?text=&url=`, Telegram `t.me/share/url?url=&text=`, Bluesky
`bsky.app/intent/compose?text=`, Reddit `reddit.com/r/{subreddit}/submit?title=&url=` (undocumented), Discourse
`{base}/new-topic?title=&body=`.

## 8. Content, rendering and counting

- **Canonical content:** a CommonMark subset (paragraphs, emphasis, links, inline code, code blocks, lists, block
  quotes; no raw HTML, no images in version 1).
- **Renderers** per markup (`plain`, `markdown`, `discord_md`, `telegram_html` with strict escaping of `< > & "`,
  `bbcode`), each returning the text and an **offset map** from rendered positions to source positions (the composer's
  colour bands need it).
- **Counters** per method: `x_weighted` (twitter-text v3 weights, every URL 23, NFC first), `graphemes` (extended
  grapheme clusters; Mastodon also counts each URL as its instance's reserved length and a remote mention by its local
  part), `utf16`, `codepoints`, `bytes` (UTF-8).
- **Rust is authoritative** (validation before enqueue); the TypeScript implementations in `content-hub-client` are for
  previews. Both must reproduce `vectors.json` (published from `content-hub-client`; this repository's copy is checked
  byte-for-byte against the published package by the contract job). The vectors carry the
  enumerations, both state machines, and the counting, rendering, segmentation and intent-link cases (generated by
  `cargo run -p content-hub-core --example gen-vectors` from `crates/core/vectors-input.json`). Offsets are Unicode
  code points in both languages.

## 9. Segmentation (the composer's colour bands)

For the selected platforms **without** an override:

- for each platform *p*, `cut_p` is the largest source position — on a grapheme boundary, with URLs and Markdown
  link constructs atomic — such that `count_p(render_p(source[0..cut_p])) ≤ limit_p`;
- sort the distinct cuts ascending, `c1 ≤ … ≤ ck`;
- segment `[0, c1)` fits every selected platform; `[ci, ci+1)` fits exactly the platforms whose cut is `≥ ci+1`
  (labelled); `[ck, end)` fits none (grey);
- a platform with an override gets its own bar and count.

Nothing is ever truncated automatically for sending: an overflowing instance cannot be queued unless it has an
override, or the operator explicitly chooses "truncate with ellipsis and link", which is itself rendered and counted.

## 10. Publishing pipeline

The operator's view (platforms, credentials, going live, egress) is [PUBLISHING.md](PUBLISHING.md).

1. **Enqueue** (admin, one transaction): for each target, render and validate the payload (limits by counting
   method, adjusted per target: a Discord embed's 4096, a Mastodon instance's own limits; escaping; secondary limits),
   store it on the instance (title, body, SHA-256, post revision), check every applicable cap and reserve the cost
   (`planned → queued`, `scheduled_at`), or `manual_required` when a cap would be passed (nothing reserved) or the
   platform is manual (with its intent link). Any operator move into `queued` repeats all of this from the current
   post.
2. **Claim** (worker): the next due row (`COALESCE(next_attempt_at, scheduled_at) <= now()`) `FOR UPDATE SKIP LOCKED`.
   In the same transaction it is stopped (`queued → manual_required`, spend released: platform or target switched
   off, a manual platform, no publisher), deferred (stays `queued` until midnight UTC: the target's posts-per-day
   limit), recorded as a **dry run** (`queued → manual_required` with a `dry_run` attempt holding the exact redacted
   request: the send would not be live), or moved to `sending` with an open attempt and **committed before the
   external call**.
3. **Send:** the stored payload, exactly once, through the checked client. Outcomes (`content_hub_core::delivery`):

   | Publisher says | Instance | Attempt | Spend | Retry |
   | --- | --- | --- | --- | --- |
   | posted (URL or id) | `posted` | `success` | settle | — |
   | refused by the platform | `failed` | `validation_error` / `failed` | release | — |
   | provably not sent (no connection, 429, failed login, an idempotent repeat) | `queued` | `rate_limited` / `failed` | keep | max(hint, 30 s·2ⁿ⁻¹ ≤ 1 h) + ≤ 20 % jitter; at most `MAX_ATTEMPTS` (5), then `failed` |
   | anything else (timeout after sending, 5xx) | `unknown` | `unknown` | keep | never |

   Mastodon is the one documented idempotent endpoint: with the same `Idempotency-Key` a timeout or 5xx is repeated
   within 45 minutes of the first attempt (the key is kept for an hour). A Bluesky login failure posts nothing, so it
   is retried. Credentials are scrubbed from every reply excerpt and reason before they are stored.
4. **Finish** (new transaction): the attempt's outcome and the instance's next state, guarded on the row still being
   this attempt's `sending`; the ledger per the table. If the janitor took the row meanwhile, the instance stays
   `unknown` and the late result is written to the audit log for the operator.
5. **Janitor:** every loop, a `sending` row older than the claim lease (`CLAIM_LEASE_SECONDS`, 600) becomes `unknown`
   (never `queued`) and its open attempt is closed. An operator resolves `unknown` to `posted` (the cost is settled) or
   `failed` (released).
6. **Modes:** live only with `PUBLISHING_LIVE=1`, the target's `live`, and production or a sandbox target; otherwise
   dry runs. `publishing.paused` stops claims. Rate limits: the platform's 429 hints; posts per target per day
   (`DAILY_POST_LIMIT`, 5, lowered by runtime caps).
7. **Observability:** `/metrics` on the worker, read from the database at scrape time: instances by status, attempts
   by platform and outcome, spend and caps, the stop switch, live mode. An optional summary to an internal Discord
   channel (`ALERT_DISCORD_WEBHOOK_URL`): new `failed` / `unknown` counts and caps crossing 50 / 80 / 100 % (once each
   per month), at most once per `ALERT_INTERVAL_SECONDS`.
8. **Periodic:** Mastodon instance limits are refreshed daily into the target's `config.instance_limits`.

## 11. Security model

| Threat | Control |
| --- | --- |
| A request reaching the origin without Access | tunnel `originRequest.access`; the backend verifies the JWT itself on every `/api` request |
| A stolen or replayed JWT | short Access token lifetime, `exp`/`nbf`/`aud`/`iss` checks, Access session revocation; Keto membership checked per request (cached briefly) |
| CSRF | custom header + `Origin` check on every state-changing route; Access cookie `SameSite=Lax` |
| Public over-exposure | the public process runs as a role that can read only the `public_v1_*` views; the views' columns are pinned by a test; public identifiers are random UUIDs |
| The public process reaching admin routes | separate router and state types; route-table tests; ingress routes per host; contract job |
| Injected mentions or markup | Discord always `allowed_mentions: {"parse": []}`; Telegram HTML escaping; renderers never pass raw HTML through; the UI never renders untrusted strings as HTML |
| Leaked platform tokens | tokens only in Kubernetes Secrets, reached by environment-variable name (`secret_ref`); the column refuses anything but a name; redaction in logs, attempts and dry runs; least-permission accounts per platform |
| SSRF through URLs the operator enters | the admin API refuses non-https URLs and internal names or addresses; the worker's client resolves names itself and connects only to public addresses it checked, follows no redirects and ignores proxies; egress allowlisted by FQDN |
| A target naming a process setting as its credential | credential names must start with `PLATFORM_` (CHECK, API, worker); the worker reads only such names |
| Double posting | at most one live instance per post and target (database); `sending` committed before the call; `unknown` never retried automatically |
| Runaway spend | layered caps, reserved before the action, hard stop at the cap |
| A compromised writer process | no DDL, no `TRUNCATE`, append-only audit log and ledger; no route to anything but its allowlisted hosts |
| Database credentials | `verify-full` TLS only; the URL and password never appear in logs, errors or `Debug` output |

## 12. Not in version 1

- Media uploads (presigned URLs on S3-compatible storage).
- An authenticated admin MCP endpoint (OAuth 2.1 against the future self-hosted identity provider).
- Approval workflows (a second operator approving an enqueue).
- The move from Cloudflare Access to the self-hosted OIDC login (only the `IdentityVerifier` implementation changes).
