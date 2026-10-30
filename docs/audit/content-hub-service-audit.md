# Security Audit — `content-hub-service`

**Classification:** Internal security review
**Project:** repos/content-hub-service (the content hub's backend: posts, public roadmap, publishing queue; one Rust
binary with the subcommands `admin`, `public`, `worker`, `migrate`, `seed`, `export-snapshot`)
**Project type:** Rust service + container image
**Template:**

- AUDIT_TEMPLATE.md (2026-10-08)
- AUDIT_TEMPLATE_RUST.md (2026-10-08) — the service is Rust
- AUDIT_TEMPLATE_DB.md (2026-10-25) — owns its schema; three database roles
- AUDIT_TEMPLATE_AUTH.md (2026-10-08) — verifies Cloudflare Access JWTs and decides authorisation with Keto (C5)
- AUDIT_TEMPLATE_IMG.md (2026-10-08) — `Dockerfile` + `docker-publish.yml` + the instance's manifests (C9)
- AUDIT_TEMPLATE_WALRUS.md (2026-09-30) — stores the signed roadmap snapshot through a Walrus publisher (C8)
- AUDIT_TEMPLATE_SUI_CLIENT.md (2026-10-08) — signs the snapshot with a Sui personal-message signature (C8)
- AUDIT_TEMPLATE_OPS.md (2026-10-08) — the snapshot CronJob spends from a hot wallet on a real chain (C8)

Not triggered: SUI (no Move code), SEAL, VUE, CF (no Worker), PX (forwards nothing), GO, SITE, HOST.

**Toolchain:** rust-version 1.94 (sqlx 0.9's floor; MSRV job)   **Edition:** 2024
**Crates published:** no (`publish = false`)
**Runtime:** tokio multi-thread (`rt-multi-thread`, `net`, `signal`, `time`)
**Database:** PostgreSQL 18, the platform's shared tier; database `content_hub`
**Roles:** owner (`migrate` Job; DDL) · writer (`admin`, `worker`, `seed`, `export-snapshot`; DML on `hub`, no DDL,
no `TRUNCATE`) · read-only (`public`; `SELECT` on schema `public` only, read-only sessions) — connection limits 2/12/10
**Migrations:** `sqlx::migrate!` (embedded, SHA-384 checksums, advisory lock), `crates/store/migrations/`, run by the
`migrate` Job before each rollout; a newer schema is refused
**Data classes:** public — the `public_v1_*` views; internal — every `hub` table; personal — `hub.users` (identity
provider subject, display name); secret-adjacent — `platform_targets.secret_ref` (a NAME only), `delivery_attempts`
(redacted excerpts)
**Backups:** the tier's daily `pg_dump` (the database is in it since 2026-10-30) with an encrypted offsite copy on R2 (14 sets,
size-guarded); restore drills passed 2026-10-30 from the in-cluster set and from the offsite set (6 databases, `content_hub` 23 tables)
**Auth role(s):** resource server / verifier (Access JWT); authorization decision point (Keto) — C5
**Identity provider:** Cloudflare Access (team JWKS); Keto read API for roles; no Hydra client
**Token formats:** JWT (RS256), `Cf-Access-Jwt-Assertion` header
**Signing / client credentials:** none held (verification only); the snapshot signing key (C8, a SOPS Secret)
**Authorization model:** Keto `ContentHub:admin#member@<subject>`
**Images:** quay.io/meddleware-org/content-hub-service (unpublished) · docker.io/meddleware/content-hub-service
**Base images:** `rust:1-slim@sha256:4df2ae8c…` (build) · `gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d…` (runtime)
**Runtime user:** 65532 (distroless nonroot)   **Runtime FS:** read-only; no volume
**Deployed by:** `post-bootstrap/content-hub/` in the operator's instance repository (C9); digest from `config/images.yaml`
**Build args:** VERSION, VENDOR, DESCRIPTION, SOURCE_URL, DOCUMENTATION_URL, IMAGE_URL — labels only, none secret
**Walrus SDK:** none (no TypeScript in the backend); the snapshot goes through an internal Walrus publisher (C8)
**Package config source:** n/a until C8   **Upload relays:** none   **Aggregator / publisher hosts:** the internal publisher (C8)
**Tip ceiling:** n/a   **Epoch default / maximum:** testnet 30 / mainnet 2 per snapshot (C8 defaults)   **Deletable default:** C8
**Sui SDK:** none yet (C8: Rust signing of a personal message)
**Transport:** n/a until C8 (the publisher submits; the service signs)
**Networks:** localnet · testnet (mainnet after the entity gate)
**On-chain packages consumed:** none (a Walrus blob object, C8)
**CLI tools:** none   **Networks targetable:** as above   **Key material:** the snapshot hot-wallet key, a SOPS Secret (C8)
**Deployment status:** unpublished (0.0.1 local, Series C C4)
**Review date:** 2026-10-25
**Reviewer:** Internal review
**Severity ceiling:** High — the service holds the operator's unreleased plans and posting credentials (by reference)
and sends to public channels; a leak of internal data or a duplicate public post is its worst outcome
**Status:** C6 — admin and public APIs, authentication, seed, text engine and the publishing worker reviewed
(2026-10-29); the snapshot export is reviewed as it lands (C8)

---

## Executive summary

The scaffold implements the parts that fix the service's security shape before any handler exists. The public
process connects as a role that **cannot read any internal table** (the database refuses it; the public views are its
only source, their columns pinned by a test). The admin and public routers have different state types, and each
answers exactly its route table (F1). Migrations are embedded, checksummed, serialised and refuse a newer schema
(F2). The publishing state machine and a unique index make a duplicate live send impossible, and an uncertain send
is never retried automatically (F3). A credential value cannot be stored where only its name belongs (F4).

Open by design (DEFERRED, each to a named step): the data routes and the Access/Keto checks (C5, F5), the worker
(C6), the first testnet store (R2, F17), the tier's `writer` record type (C9, F7). The contract job needs the
client published first (R2, F8). Nothing is deployed.

## Threat model / trust boundaries

| Actor | Holds / proves | Can do | Bounded by |
| --- | --- | --- | --- |
| Operator (admin) | an Access session; a Keto role | everything the admin API offers | Access at the edge; JWT and Keto checked by the backend (C5); CSRF; audit log |
| Anonymous reader of `roadmap.` / `feeds.` | nothing | GET the public routes | the public process's role (public views only); GET only; rate limits (C9) |
| The worker | the writer role; platform tokens (env, by name) | send posts | the instance state machine; spend caps; egress allowlist (C9) |
| A platform (Discord, Telegram, …) | its responses | report success, failure or nothing | `unknown` on doubt; redaction of excerpts (C6) |
| The `migrate` Job | the owner role | change the schema | embedded, reviewed, checksummed migrations |
| The database tier operator | superuser | everything | trust assumption (platform lens) |
| Cloudflare Access | the identity assertion | admit a user | the backend verifies the JWT itself (aud, iss, exp, signature) |

**Primary trust anchors:** the database grants (what each process can touch) and the Access JWT verification.

### Threat model — service actor matrix (RUST lens)

| Actor | Controls | Bounded by |
| --- | --- | --- |
| Network client | request line, headers, body | axum routing (unknown → JSON 404); body limits and timeouts (C5) |
| Upstream platform | response size, status, timing | bounded reads and timeouts per publisher (C6) |
| Shared state store | — | PostgreSQL only; no Redis/Dragonfly |
| Ingress | forwarding headers | rate limits at the ingress (C9); no IP-keyed logic in the service yet |
| Crate registry and build | compiled code | `Cargo.lock`, cargo-deny, SBOM gate |

### Threat model — database actor matrix (DB lens)

| Actor / asset | Power | Bounded by |
| --- | --- | --- |
| `public` process | read-only role | `USAGE`/`SELECT` on schema `public` only (F1) |
| `admin`, `worker`, `seed`, `export-snapshot` | writer role | DML on `hub`; no DDL, no `TRUNCATE`; append-only audit log and ledger |
| `migrate` Job | owner | migrations (F2) |
| Request authors | query values | SQLx bound parameters; dynamic SQL needs `AssertSqlSafe` |
| A developer adding a public column | exposure | `public_view_columns_are_exactly_the_documented_ones` |
| Backups | full copy | tier backup + offsite (C9, OPERATOR_TASKS) |

### Identity & credential matrix (AUTH lens)

| Authority / credential | Holder | What it confers | Misuse / compromise impact | Rotation / revocation plan |
| --- | --- | --- | --- | --- |
| Access JWT (RS256) | Cloudflare; the operator's browser | admin session | admin actions until expiry | Access session revocation; short lifetime |
| Keto relation `ContentHub:admin#member` | Keto | the admin role | admin actions | delete the tuple |
| Platform tokens / webhook URLs | Kubernetes Secrets (env, by name) | posting as the operator | posts on the operator's channels | rotate per platform; `secret_ref` points at the new name |
| Database passwords (three roles) | tier Secrets | per-role access | per-role (F1) | `new-service-db-secret.sh` rotation |

### Image supply chain (IMG lens)

| Actor / asset | Power | Bounded by |
| --- | --- | --- |
| Base-image publishers (rust, distroless) | everything in the layers | digest pins; distroless runtime |
| Build context | files copied in | `.dockerignore` (no `target`, `.git`, `.env*`) |
| Build arguments | labels | no secrets |
| Registry | the manifest for a tag | digest pinning; cosign bundle verification at admission (D81) |

### Walrus, Sui and operations matrices (WALRUS, SUI_CLIENT, OPS lenses)

The snapshot export, its signature and the publisher client exist (C8, ADR-0007): the signature is proved against the Sui
SDK (I20) and the export's bounds against PostgreSQL (I21). Not yet exercised: a store through the real publisher and the
hot wallet's float and alert (R2, F17).

## Severity scale

Critical / High / Medium / Low / Info / Positive (unchanged across the corpus).

## Scope

In scope: `crates/*`, `crates/store/migrations/0001_init.sql`, `seed/platform-definitions.json`,
`schema/roadmap-seed.schema.json`, `vectors.json`, `Dockerfile`, `.github/workflows/*`, `scripts/*`, `tests/contract`.
Out of scope: the instance manifests (C9), Cloudflare and Keto configuration (operator).

Environment: `cargo` stable 1.98.1 (`SQLX_OFFLINE=true`), PostgreSQL 18 with TLS (`scripts/test-postgres.sh`,
podman), podman 4.9.3 for the image and `scripts/smoke.sh`, Node 24 for the contract test (against a locally packed
`@meddleware/content-hub-client` 0.0.1).

## Findings

### F1 — Positive: the public process cannot read internal data, and each router serves only its table
**Severity:** Positive
**Where:** `crates/store/migrations/0001_init.sql` (schema `hub`, views `public.public_v1_*`); `crates/store/src/testdb.rs`;
`crates/api/src/{public,admin,routes}.rs`
**Issue:** —
**Impact:** A bug in a public handler cannot leak an internal field: the read-only role has no privilege on schema
`hub` (permission denied), not even through the auto-updatable views; public identifiers are random UUIDs, not keys.
The public router cannot mount an admin handler (different state type) and 404s every admin route.
**Remediation / evidence:** store tests `the_readonly_role_reads_only_the_public_views`,
`public_view_columns_are_exactly_the_documented_ones`, `visibility_follows_both_the_item_and_its_phase`,
`the_feed_lists_only_posted_entries_on_enabled_feed_targets`; api tests `the_public_router_serves_its_table_and_no_admin_route`,
`the_admin_router_serves_its_table_and_no_public_route`, `the_public_router_refuses_every_write_method`; `scripts/smoke.sh`.

### F2 — Positive: migration safety
**Severity:** Positive
**Where:** `crates/store/src/migrate.rs`
**Issue:** —
**Impact:** Two Jobs never apply a migration twice; an edited released migration and a database from a newer build are
refused; application roles cannot migrate (smoke test: the writer's `migrate` fails).
**Remediation / evidence:** tests `migrations_apply_once_and_report_the_version`, `racing_migrators_apply_each_migration_once`
(which found and fixed a reporting race in the first draft: both racers reported "applied"; the run is now under its own
advisory lock), `a_schema_from_a_newer_build_is_refused`, `an_edited_migration_is_refused`,
`the_first_migration_creates_the_data_schema_when_provisioning_did_not`.

### F3 — Positive: at most one live send per post and target; uncertain sends are never retried automatically
**Severity:** Positive
**Where:** `crates/core/src/instance_state.rs`; `platform_instances_one_live` (unique partial index);
`platform_instances_sending` check
**Issue:** —
**Impact:** The database refuses a second live instance; only the worker enters `sending`; only an operator leaves
`unknown`, to `posted` or `failed`; `posted` is terminal.
**Remediation / evidence:** core tests enumerate every (actor, from, to) triple against the documented table; store test
`an_instance_is_live_at_most_once_per_target_and_keeps_its_state_consistent`; the shared vectors pin the same table for
the TypeScript client.

### F4 — Positive: a credential value cannot be stored where its name belongs
**Severity:** Positive
**Where:** `platform_targets.secret_ref` CHECK `^[A-Z][A-Z0-9_]{0,63}$`; `content_hub_core::ident::is_secret_ref`
**Issue:** —
**Impact:** A webhook URL or token pasted into the target form is refused by the database, so it never reaches a
backup, the audit log or an API response.
**Remediation / evidence:** store test `constraints_refuse_inconsistent_rows` (a Discord webhook URL is refused); core test
`secret_refs_are_names_never_values`.

### F5 — Admin authentication: Access JWT, Keto role, CSRF — implemented and fail-closed
**Severity:** Positive (was Info, DEFERRED)   **Disposition:** RESOLVED (2026-10-29)
**Where:** `crates/api/src/auth/{mod,jwt}.rs`, `crates/api/src/admin/mod.rs` (`route_layer(require_admin)`)
**Issue:** —
**Impact:** Every admin route needs a valid Access JWT (RS256 with ring against the team JWKS; `aud`, `iss`, `exp`,
`nbf` with bounded skew; the JWKS cached ≤ 1 h and refreshed on an unknown `kid` at most every 30 s), a Keto
`ContentHub:admin#member` role (allowed cached 30 s, denied 5 s) and, for writes, the CSRF header plus the admin
`Origin`. Keto or the JWKS host unreachable → 503; a garbage token → 401, never 500. The development verifier is
refused unless `CONTENT_HUB_ENVIRONMENT=development`.
**Remediation / evidence:** `jwt::tests::{every_defect_is_refused_with_its_reason, forged_tokens_are_refused}`,
`handler_tests::{the_auth_matrix, keto_and_jwks_outages_fail_closed}`; smoke: `/api/v1/me` and `/api/v1/phases`
without a token → 401 (image).

### F6 — Snapshot, signing and hot wallet (C8)
**Severity:** Info   **Disposition:** RESOLVED (implemented; the live parts are F17)
**Where:** `core::snapshot`, `publish::blob`, `worker::snapshot`, `content-hub export-snapshot|snapshot-key|snapshot-address`,
`GET /public/v1/snapshot`
**Issue:** The WALRUS, SUI_CLIENT and OPS lens rows had nothing to bind to.
**Impact:** None now.
**Remediation / evidence:** canonical JSON and the Sui personal-message signature checked against `@mysten/sui` in the
contract job (`tests/contract/snapshot.test.ts` on a committed fixture signed by a test key); the export's interval,
renewal, lock and no-record-on-failure rules against PostgreSQL and a fake store; the publisher client against a mock.
The signer holds nothing (ADR-0007 §1); the wallet that pays belongs to the internal publisher, not the hub.

### F7 — The database tier's `writer` record type
**Severity:** Low   **Disposition:** RESOLVED (2026-10-29, C9; deployed in R2)
**Where:** the platform's `k8s/base/database/scripts/provision.sh` (instance repository)
**Issue:** The writer role and the data schema are provisioned today only by the test harness and the smoke script.
**Impact:** The service cannot be deployed on the tier until the record exists.
**Remediation / evidence:** C9 adds `writer … schema=<name>` (USAGE + default DML on the data schema, SELECT on
`public`) with `--check` validation and `harden-backup-test.sh` cases, per ARCHITECTURE.md §6 "Roles".
Done in the deployment workspace: `provision.sh` gained `writer <service> <role> schema=<s>` (DML on the data schema's
tables and sequences, now and by default privileges; SELECT on `public`; never CREATE/TRUNCATE; a self-check fails the run
otherwise); the tier test proves it against PostgreSQL 18 (writer DML on tables created later, no DDL/TRUNCATE/grants, no
other database, read-only role sees nothing of `hub`, convergence) and the local end-to-end run migrates and serves this
service on roles made by the real `provision.sh`.

### F8 — The contract job installs a client that is not published yet
**Severity:** Low   **Disposition:** RESOLVED (2026-10-30, R2)
**Where:** `.github/workflows/rust-ci.yml` (`contract`), `tests/contract/package.json` (no lockfile yet)
**Issue:** `@meddleware/content-hub-client` 0.0.1 is unpublished, so `npm ci` in the contract job cannot run.
**Impact:** CI's contract job fails until the client is published.
**Remediation / evidence:** R2 order: publish the client first, then generate `tests/contract/package-lock.json` and
push the service. The test itself passed locally against a packed client (3/3).
Done: the client is on npm (0.0.1, with provenance), the lockfile exists, and the contract job is green on `main` and on the tag.

### F9 — Platform facts corrected against the research brief
**Severity:** Info   **Disposition:** RESOLVED
**Where:** `seed/platform-definitions.json`
**Issue:** The brief had Mastodon counting code points and the X intent path `/intent/post`.
**Impact:** Wrong previews and a broken manual-assist link.
**Remediation / evidence:** Mastodon counts grapheme clusters (`StatusLengthValidator`), URLs as 23, a remote mention by
its local part; X's documented intent is `x.com/intent/tweet` (docs.x.com Web Intent). Both rows changed; checked
2026-10-25. Discord's character unit is undocumented: code points assumed, to confirm with a sandbox send (C6).

### F10 — URLs are checked for public hosts at the edge; the resolved address is checked only where it is fetched
**Severity:** Low   **Disposition:** RESOLVED (2026-10-29, C6: `publish::net` resolves names itself, drops every non-public
address before connecting, refuses non-public IP literals, plain http, redirects and proxies; Mastodon instance URLs and
Bluesky `pds` are validated at the admin API too; the egress FQDN policy follows in C9)
**Where:** `crates/api/src/admin/mod.rs` `https_url` (task links, posted URLs, external references)
**Issue:** The admin API refuses non-`https` URLs, credentials in URLs, internal names (`localhost`, single labels,
`.local`, `.internal`, `.svc.cluster.local`, …) and private, loopback, link-local, shared (100.64/10), benchmark,
documentation, multicast and IPv4-mapped private addresses. A public name can still resolve to a private address.
**Impact:** None today: the admin process never fetches these URLs (they are links shown to the operator). It matters
for anything that fetches, i.e. the C6 worker (Mastodon instance limits, target base URLs).
**Remediation / evidence:** `admin::tests::https_urls_must_name_public_hosts`. C6: resolve, check every address with the
same rules, connect to the checked address, follow no redirect to a new host; plus the egress FQDN allowlist (C9).

### F11 — Paid platforms cannot be enqueued until spend reservation exists
**Severity:** Info   **Disposition:** RESOLVED (2026-10-29, C6: enqueue reserves under the total, platform and target caps
behind an advisory lock, or the instance is `manual_required`; the worker settles or releases; operator moves follow)
**Where:** `crates/api/src/admin/posts.rs` `enqueue`
**Issue:** The spend ledger and caps exist (budgets API, ceilings from the instance config), but reserving spend at
enqueue time belongs with the worker that settles it.
**Impact:** Safe by construction: a target whose platform has a non-zero cost model is refused with a reason; every
default-enabled platform is free.
**Remediation / evidence:** C6 reserves the estimate in `spend_ledger` under the caps in the enqueue transaction and
settles or releases it on the outcome; `budgets_respect_the_instance_ceilings` covers the caps API today.

### F12 — Positive: at most once, end to end
**Severity:** Positive
**Where:** `crates/worker/src/lib.rs`, `crates/store/src/repo/delivery.rs`, `crates/core/src/delivery.rs`
**Issue:** —
**Impact:** `sending` is durable before the call; a send that may have gone out is `unknown` and is never claimed
again; `Retry` is reported only for provably unsent requests (no connection, 429, a failed login, Mastodon's documented
idempotent repeat within 45 minutes); a stale `sending` becomes `unknown` (janitor) and a late result cannot overwrite
it (guarded finish; the result goes to the audit log); claims are exclusive (`SKIP LOCKED`).
**Remediation / evidence:** worker tests (I7, I15), `claims_are_exclusive`,
`the_janitor_turns_stale_sends_into_unknown_and_late_results_are_kept_for_the_operator`, the decision table tests.

### F13 — Positive: nothing is sent by default
**Severity:** Positive
**Where:** `worker::live`, `PUBLISHING_LIVE`, `platform_targets.live`, `publishing.paused`
**Issue:** —
**Impact:** a new deployment, a misconfigured target or a development environment produces dry runs (the exact
redacted request, handed to a person); outside production only sandbox targets can be live; the stop switch halts
claims at once.
**Remediation / evidence:** I17 tests; the end-to-end test (a live feed target, a non-live Discord target: no request
left the process).

### F14 — Credential hygiene, including a gap found and closed during C6
**Severity:** Low (fixed before any release)   **Disposition:** RESOLVED (2026-10-29)
**Where:** `core::platform::{scrub, scrub_sent, excerpt}`, `publish::report`
**Issue:** The first publisher tests showed that a platform error quoting the credential (a Telegram 401 echoing the
token) was scrubbed from the stored excerpt but not from the outcome's reason, which becomes `last_error`; and the
excerpt clipped before scrubbing, which could keep part of a secret at the cut.
**Impact:** None in practice (unreleased, mocks only); had it shipped, an admin-visible field could have shown a token.
**Remediation / evidence:** reasons are scrubbed like excerpts, scrubbing happens before clipping, Bluesky access tokens
are scrubbed too; `excerpts_never_carry_the_secret`, `telegram_outcomes_and_scrubbed_errors`, and a `no_secret_in`
check in every publisher test. Credential names must start with `PLATFORM_` (I10).

### F15 — Facts only a live send can confirm
**Severity:** Info   **Disposition:** DEFERRED (to the first live send per platform)
**Where:** docs/PUBLISHING.md §3
**Issue:** The publishers follow the documented request and reply shapes (each fixture names its source), but some facts
are undocumented or account-specific: Discord's character unit, Bluesky's acceptance of a TID record key from the first
attempt's time, a private Telegram channel's message link, a Mastodon instance's own limits.
**Impact:** A wrong assumption shows as a `failed` (refused) send, never a double post.
**Remediation / evidence:** the first live send per platform goes to a private target owned by the publishing
organisation, before any public target (operator decision D85).

### F16 — The worker's egress allowlist
**Severity:** Low   **Disposition:** RESOLVED (2026-10-29, C9; enforced from R2)
**Where:** the namespace's CiliumNetworkPolicy
**Issue:** The client refuses non-public addresses, but the namespace does not yet restrict which public hosts it reaches.
**Impact:** A compromised worker could reach any public host.
**Remediation / evidence:** C9's default-deny policy with the FQDN list in docs/PUBLISHING.md §4. The deployment's
namespace denies ingress and egress by default; Cilium policies allow the worker only `discord.com`, `api.telegram.org`
and `bsky.social` on 443 (a Mastodon instance is added with its target), the admin only Keto and the Access JWKS host,
the export Job only the publisher, everyone PostgreSQL and DNS; proved on the rendered manifests by the deployment's
`route_exposure.py`. Live enforcement is checked in R2 (Hubble, no drops expected).

### F17 — Facts only the first testnet store can confirm
**Severity:** Low   **Disposition:** RESOLVED (2026-10-30, R2)
**Where:** `publish::blob` (token claims), `post-bootstrap/_prepared/walrus-storage-publisher` (instance repository)
**Issue:** The publisher's acceptance of the hub's token (`exp`, `iat`, `jti`, exact `epochs` and `size`), the reply shape
for `newlyCreated`/`alreadyCertified`, and `--jwt-verify-upload` are taken from Mysten's source and CLI help, proved against
a mock only (ADR-0007 OQ1).
**Impact:** The first real export could be refused; nothing is recorded in that case (I21).
**Remediation / evidence:** R2 (2026-10-30): the first store through the real publisher on Walrus testnet succeeded with the hub's token;
the blob fetched from the public aggregator hashed to the published SHA-256 and its signature verified with `@mysten/sui` to the published
signer; `--jwt-verify-upload` was then enabled and a second real store (the renewal path) succeeded under it; the reply shape
(`newlyCreated`/`alreadyCertified`) parsed. The daily export is on.

### F18 — The publisher client may call only its configured host
**Severity:** Info   **Disposition:** Positive
**Where:** `publish::net::Http::internal`, `publish::blob`
**Issue:** The hub must reach an in-cluster HTTP service, which the checked public-only client forbids.
**Impact:** A separate client mode is the one place plain `http` and a private address are allowed; widening it would be an SSRF path.
**Remediation / evidence:** the mode is built for one host from `WALRUS_PUBLISHER_URL` (bare origin only), still refuses
redirects and proxies, and is used by nothing else (`the_publisher_url_must_be_a_bare_origin`, `publish::tests`).

### F19 — Snapshot signer custody
**Severity:** Info   **Disposition:** Positive
**Where:** `snapshot-key`, `SNAPSHOT_SIGNER_SEED`, `core::snapshot::Signer`
**Issue:** A signing key in the cluster must not become a spending key or leak through diagnostics.
**Impact:** A leaked seed forges snapshots (rotation and notice, ADR-0007 §5), never moves funds.
**Remediation / evidence:** the seed file is created 0600, never overwritten, and only the address is printed; `Debug` of
the signer shows the address only; the seed is read from the environment only in the export Job; the smoke script proves the
address-only output.

## Section A — Invariant verification matrix

| # | Invariant (plain statement) | Enforced / asserted at (`file:fn`) | Proven by (test / code-assert / cross-project) | Status |
| --- | --- | --- | --- | --- |
| I1 | The public process can read only the public views | grants (`testdb.rs`; tier `readonly`) | `the_readonly_role_reads_only_the_public_views` | HOLDS |
| I2 | Public views expose exactly the documented columns | `0001_init.sql` views | `public_view_columns_are_exactly_the_documented_ones` | HOLDS |
| I3 | An item is public only if its phase is public to the same audience | `public_v1_items` | `visibility_follows_both_the_item_and_its_phase` | HOLDS |
| I4 | Application roles cannot change the schema or truncate | grants | `the_writer_changes_data_but_never_the_schema`; smoke | HOLDS |
| I5 | Migrations: once, serialised, checksummed, no downgrade | `migrate.rs::run` | F2 tests | HOLDS |
| I6 | At most one live instance per post and target | `platform_instances_one_live` | store test (F3) | HOLDS |
| I7 | Only the worker enters `sending`; `unknown` left only by an operator | `instance_state::allowed`; `delivery::decide`; `repo::delivery` guards | core tests (every triple; every decision is a worker move); worker tests (`unknown` never reclaimed) | HOLDS |
| I8 | Audit log and spend ledger are append-only | triggers | `audit_log_and_spend_ledger_are_append_only` | HOLDS |
| I9 | Enumerations agree in SQL, Rust and the vectors | CHECKs, `model.rs`, `vectors.json` | `every_check_constrained_enumeration_matches_the_rust_enum`; `vectors_tests` | HOLDS |
| I10 | `secret_ref` holds a `PLATFORM_` name, never a value or another setting | CHECK; `ident::is_secret_ref`; `env_secrets` | `constraints_refuse_inconsistent_rows`; `secret_refs_are_names_never_values`; `a_missing_credential_fails_without_a_call` | HOLDS |
| I11 | Database TLS is verify-full; the password never prints | `config.rs::parse`, `Debug` | config tests | HOLDS |
| I12 | Each router serves exactly its table | `routes.rs`, routers | api tests; contract job | HOLDS |
| I13 | Admin requests need a valid Access JWT, a Keto role and CSRF | `auth::require_admin` | `the_auth_matrix`; `keto_and_jwks_outages_fail_closed` | HOLDS |
| I14 | Spend is reserved before the action and capped | `admin::prepare::over_cap`; `repo::spend` | `paid_sends_reserve_spend_and_every_requeue_revalidates`; `spend_is_settled_when_sent_and_released_when_refused` | HOLDS |
| I15 | `sending` is committed before any external call | `Worker::tick` (commit before `send`) | `sending_is_committed_before_the_call_and_the_post_is_recorded` (the mock reads the row mid-request) | HOLDS |
| I16 | Nothing is queued or sent without a validated payload; the worker sends that payload | CHECK `platform_instances_payload_before_send`; `prepare` | store and worker tests; e2e | HOLDS |
| I17 | Nothing is sent unless `PUBLISHING_LIVE=1`, the target is live, and production or a sandbox target | `worker::live` | `live_rules`; dry-run and sandbox tests; e2e | HOLDS |
| I18 | The worker connects only to public addresses it resolved, over https, without redirects or proxies | `publish::net` | `the_production_client_refuses_private_and_plain_targets` | HOLDS |
| I19 | The snapshot is canonical (same data, same bytes and hash) and holds only what the public routes serve | `core::snapshot`, `worker::snapshot` | `canonical_json_*`; `unchanged_data_gives_the_same_bytes_and_is_not_stored_again`; `internal_wording_never_reaches_the_signed_document` | HOLDS |
| I20 | The signature is a standard Sui personal-message signature | `core::snapshot` | the fixture tests; `snapshot.test.ts` (`@mysten/sui`) | HOLDS |
| I21 | A snapshot is recorded only after its bytes are stored; at most one new snapshot per interval; a second run waits | `worker::snapshot` | `a_failed_store_records_nothing`; `a_change_waits_for_the_minimum_interval`; `two_exports_do_not_run_at_once` | HOLDS |
| I22 | Mainnet snapshots are off unless explicitly allowed; the signer holds nothing | `cli` | `mainnet_snapshots_are_refused_unless_allowed`; design (ADR-0007) | HOLDS |

## Section B — Supply-chain, publish-authority & capability matrix

### B.1 Dependency & CVE risk

| Dependency | Pinned version / rev / object ID | Liveness dependency? | CVE / audit status | Notes |
| --- | --- | --- | --- | --- |
| `sqlx` (postgres, rustls-ring-webpki, macros, migrate) | =0.9.0 | every database path; fails closed (503) | cargo-deny clean 2026-10-25 | no MySQL/SQLite drivers |
| `axum` / `tower` | =0.8.9 / =0.5.3 | HTTP | clean | |
| `tokio` | =1.54.1 | runtime | clean | |
| `rustls` (ring) | 0.23.46 (lockfile) | database TLS | clean | one TLS stack |
| `clap`, `tracing`, `tracing-subscriber` | =4.6.7, =0.1.44, =0.3.23 | — | clean | |
| PostgreSQL (tests) | `postgres:18@sha256:06cad38a…` | tests only | — | same image as the tier |
| Cloudflare Access JWKS | team endpoint | admin auth (C5); fails closed | — | |
| Keto read API | in-cluster | admin auth (C5); fails closed | — | |
| `reqwest` (rustls, no provider; ring installed) | =0.13.5 | JWKS, Keto, every platform send | clean | system proxies ignored; own resolver |
| `ring` | =0.17.14 | JWT (RS256); X OAuth 1.0a (`HMAC_SHA1_FOR_LEGACY_USE_ONLY`, required by the protocol) | clean | no second crypto stack |
| Platform APIs (Discord, Telegram, Bluesky PDS, Mastodon instances; X disabled) | — | sends only; an outage leaves posts `queued` (retry) or `unknown` | — | dry runs need none |

cargo-deny: advisories ok, bans ok (duplicate-version warnings only), licenses ok, sources ok (2026-10-25).

### B.2 Publish authority, capabilities & secret custody

| Authority / capability / secret | Where minted / held | Custody | Gates | Immutability / rotation plan |
| --- | --- | --- | --- | --- |
| Image publish (quay.io, Docker Hub) | CI, `docker-publish.yml` | registry robot secrets (operator) + keyless cosign | releases | robot tokens rotated by the operator |
| Database roles' passwords | tier Secrets | SOPS, operator | database access | `new-service-db-secret.sh` |
| Platform tokens | instance Secrets | SOPS, operator | posting | per platform |

#### CI & release integrity

- **Actions pinned** — holds (every action by full SHA, copied from sui-indexer).
- **Least privilege** — holds (`contents: read` default; `id-token`/`attestations` only on the signing job).
- **OIDC trusted publishing** — n/a for crates (not published); images signed keyless; registry robot credentials
  are the operator's inventory (base §B.2).
- **Tag-gated, idempotent publish** — holds (`v*` tags; dry run on manual dispatch).
- **Release gate equals CI** — holds (`verify` calls `rust-ci.yml`).
- **Automated dependency updates** — holds (`.github/dependabot.yml`: cargo, npm `/tests/contract`, docker, actions).
- **Container images** — see B.IMG-1.
- **Secrets never echoed** — holds.
- **Real funds are manual** — n/a until C8 (the snapshot CronJob spends float only, under signer caps).
- **Test-only modes** — `ALLOW_INSECURE_DATABASE=1` (plaintext database, local tests) must stay unset in production
  manifests; C9 adds a manifest check (Section D). The dev identity verifier (C5) is refused when the production
  Access configuration is set.

### B.RS-1 Build & release

fmt, clippy `-D warnings` and the MSRV job (1.94) in CI; release profile `overflow-checks = true`, `panic = "abort"`;
image per B.IMG-1; no crates published.

### B.DB-1 Roles and grants

| Role | Process | Database privileges | Schema privileges | Table privileges | Limits | Evidence (test) |
| --- | --- | --- | --- | --- | --- | --- |
| owner | `migrate` | CONNECT, TEMPORARY, CREATE; owner | owns `hub`, CREATE on `public` | owner | 2 conns, 60 s | smoke (migrates) |
| writer | `admin`, `worker`, `seed`, `export-snapshot` | CONNECT | USAGE on `hub`, `public` | SELECT, INSERT, UPDATE, DELETE on `hub`; SELECT on `public` | 12 conns, 30 s | `the_writer_changes_data_but_never_the_schema` |
| read-only | `public` | CONNECT | USAGE on `public` | SELECT on `public` (views, migrations table) | 10 conns, 5 s, read-only | `the_readonly_role_reads_only_the_public_views` |

### B.DB-2 Schema change process

A new migration file; `cargo sqlx prepare` regenerates `.sqlx/` (CI checks it against a migrated database); a changed
CHECK list changes `core::model`, the vectors (client first) and the parity tests; a changed view changes the pinned
column list, the response types and ARCHITECTURE.md §6. Every migration keeps the previous release's queries working.

### B.IMG-1 Publish & attestation

Native per-architecture builds to OCI archives without registry credentials, index merge, Trivy by digest before
cosign, SPDX + the binary's CycloneDX + build provenance, `mirror-private` by digest; dry run by default
(`docker-publish.yml`, unrun until R2). Locally: image built with podman, `scripts/smoke.sh` passed (2026-10-25).

## Section C — Test-coverage & hermetic/live split

### C.1 Coverage grade

| Dimension | Assessment |
| --- | --- |
| Happy-path coverage | migrations, probes, routes, state machines, definitions, every admin resource, enqueue, the public roadmap and feeds, the seed import, the text engine |
| Error-path / abort-code / failure-mode coverage | constraint violations by SQLSTATE; forbidden role actions; newer/edited schema; unreachable database |
| Boundary / edge-case coverage | identifier shapes at their length limits; racing migrators; cancelled vs live instances |
| Security-relevant coverage | role separation, exposure columns, visibility, secret-name refusal, route exposure, the auth matrix and outages, CSRF/Origin, public key-path snapshots, URL host checks, XML escaping |

193 Rust tests on 2026-10-29 after C8 (the snapshot, publisher client and fixture tests added; the database tests run against
PostgreSQL 18 with TLS and the production roles; the publisher and worker tests against local mocks), 0 skipped or
ignored; contract tests 6/6 (routes, vectors, snapshot signature with `@mysten/sui`) against a packed local client; `scripts/smoke.sh` — see the log.

### C.2 Hermetic vs. live paths

| Path | Hermetic unit test? | Deferred to | Tracking |
| --- | --- | --- | --- |
| Tier provisioning with the `writer` record | yes: the tier test and the local end-to-end run (C9) | — | F7 |
| Restore drill of `content_hub` | no | after the first backup (R2) | Section D |
| Access JWT against the real team JWKS | test JWKS and a static key (C5) | R2 | F5 |
| SSRF check on resolved addresses | yes (`publish::net` tests) | — | F10 |
| Real platform sends | mocks only (D85) | the first live send per platform | F15 |
| Storing a blob through the publisher | mock publisher, then the live testnet publisher (2026-10-30) | — | F17 |
| The egress FQDN policy | rendered and checked (C9) | live enforcement in R2 | F16 |
| Platform sends | mocks (C6) | optional sandbox channels | — |

## Section D — Deployment-readiness gates

### pre-localnet
- [x] builds; fmt, clippy `-D warnings`, cargo-deny green — 2026-10-25
- [x] tests on the real engine with TLS and the production roles, none skipped — 164/164 (2026-10-29, C6)
- [x] no secrets in source; `.env*` ignored
- [x] data routes, auth and CSRF — F5 (2026-10-29)
- [x] the worker sends at most once, dry-run by default, behind caps — F11–F14 (2026-10-29)

### pre-testnet
- [x] `SECURITY.md` present
- [x] public responses can come only from public views (role-enforced), columns pinned — F1
- [x] the tier's `writer` record — F7 (2026-10-29, C9)
- [x] CI green on GitHub, including the contract job against the published client — F8 (2026-10-30, R2)
- [x] manifests do not set `ALLOW_INSECURE_DATABASE`; namespace default-deny with the worker's FQDN allowlist — F16 (2026-10-29, C9)
- [x] a local end-to-end dry run of the image on tier-provisioned roles, nothing sent — C9 (2026-10-29)
- [x] the manifests applied and the FQDN policy observed enforcing (a worker-labelled probe pod reached Discord and Telegram and was blocked from example.org and github.com) — 2026-10-30, R2
- [ ] the first live send per platform to a private target, before any public one — F15 (go-live)
- [x] image published, signed (cosign, keyless), pinned by consensus — 2026-10-30, R2

### pre-mainnet
- [x] restore drill of `content_hub` with the offsite copy — PLAT-M4 / DB-M9 (2026-10-30: in-cluster and offsite sets, 23 tables restored, roles reconverged)
- [ ] snapshot signer caps and hot-wallet float proved on testnet (done); the publisher balance alert wired in Grafana — ADR-0007 §3 (open)
- [x] personal-data inventory (`hub.users`) and deletion path written — DB-M10 (docs/PERSONAL_DATA.md, scripts/erase-person.sql, smoke step 9; 2026-10-30)
- [ ] external review

## Cross-project themes

- **Supply chain & release integrity** — `Cargo.lock` committed; actions and base images pinned by digest; cargo-deny
  and the SBOM gate in CI.
- **Wire-format coupling & conformance vectors** — `vectors.json` is published from `@meddleware/content-hub-client`;
  this repository's copy is checked byte-for-byte by the contract job; enumerations and both state machines are pinned
  on both sides (counting/rendering/segmentation join in C5).
- **On-chain-truth boundary** — n/a until C8 (the snapshot is signed off-chain data with an on-chain blob).
- **Deployment readiness** — Section D.
- **Chain-access layering** — no Move package is consumed.

## Normative requirements

- **RS-M1** no `unsafe` (`forbid` per crate), no panic on request paths — holds for the scaffold.
- **RS-M5/M8** timeouts and log hygiene — partly: database acquire timeout 5 s, statement timeouts per role; no
  outbound HTTP yet; logs carry no URL or password (config tests).
- **DB-M1** holds (F2). **DB-M2** stated in `migrate.rs` and CLAUDE.md; first applies at migration 2.
- **DB-M3** holds (B.DB-1). **DB-M4** holds (SQLx binds; dynamic SQL only in the test harness, `AssertSqlSafe`).
- **DB-M5** holds (F1). **DB-M6** holds (F3, F4, I8, I9). **DB-M7** holds (I11).
- **DB-M8** holds (`/readyz` 503 when unreachable or behind; `/healthz` without the database; smoke test).
- **DB-M9** MUST before mainnet (no drill yet). **DB-M10** MUST before mainnet.
- **AUTH** MUSTs apply from C5 (F5).

## Implementation suggestions

- Add query-plan tests for the worker's claim query and the public roadmap query once they exist (DB lens §C).
- Emit `X-Robots-Tag: noindex` from the admin process as well as the ingress (defence in depth).

## Open questions

- **OQ1** Should public roadmap items show dates (planned or due)? Version 1 exposes none (they can reveal sequencing). (Decided 2026-10-25: optional — an item may carry a separate `public_due_at`, e.g. a launch time, shown only when set; internal dates stay private — see I3 and `only_the_public_due_date_is_published`.)
- **OQ2** Should feed entries carry tags or categories? Version 1 exposes neither.

## Risks

- Shared fate with the database tier (also the identity stack's); the public site degrades to the last snapshot (C8).
- Cloudflare Access and Keto availability gate admin work (fail closed).
- Platform behaviour (rate limits, counting units) changes outside our control; the definitions record the date they
  were checked.

## Re-verification log

- 2026-10-29 — C6: the publishing worker and publishers (F10, F11 RESOLVED; F12–F13 Positive; F14 found by the new tests
  and fixed before release; F15, F16 DEFERRED); invariants I7, I10, I13, I14 now proven, I15–I18 added. 164 Rust tests,
  fmt, clippy `-D warnings`, cargo-deny, `sqlx prepare --check`, contract 3/3 (with the two new admin routes); image
  built with podman and `scripts/smoke.sh` ok (the worker's probes and /metrics, dry-run by default, a bad
  PUBLISHING_LIVE refused).

- 2026-10-29 — C5: admin and public APIs, Access/Keto/CSRF (F5 RESOLVED), seed import (YAML/JSON, idempotent; the
  private seed applies twice with identical counts), the text engine and its vectors (Rust generates; the client's
  TypeScript port reproduces them). New F10 (URL host checks; resolved-address check in C6) and F11 (paid platforms
  refused until spend reservation, C6). fmt, clippy `-D warnings`, cargo-deny (advisories, bans, licences, sources ok;
  duplicate-version warnings from sqlx, reqwest and ring only), `sqlx prepare --check`, 105/105 tests, contract 3/3,
  image built with podman and `scripts/smoke.sh` ok (data routes 200, admin 401 without a token, seed as writer twice,
  refused as read-only).

- 2026-10-25 — OQ1 decided by the maintainer: optional public date per item (`public_due_at`, NULL by default); view and seed schema extended in the unreleased migration 1; new test `only_the_public_due_date_is_published`.
- 2026-10-25 — first pass of the C4 scaffold: schema version 1, migrations, roles, route tables, state machines,
  platform definitions; 59 tests, smoke test and contract test (local) pass; F1–F4 Positive, F5–F8 DEFERRED, F9 RESOLVED.
- 2026-10-29 — C8: the roadmap snapshot (canonical JSON, Sui personal-message signature, Walrus store through the internal
  publisher): F6 RESOLVED, F17 DEFERRED, F18–F19 Positive; invariants I19–I22. 193 Rust tests, fmt, clippy `-D warnings`,
  cargo-deny, `sqlx prepare --check`, contract 6/6 (the snapshot route in the client's table; its signature verified by
  `@mysten/sui`).
- 2026-10-29 — C9: deployment prepared in the operator's workspace (`post-bootstrap/_prepared/content-hub`): F7 and F16
  RESOLVED; Section D pre-testnet gates updated. Local end-to-end dry run (the image on roles from the real tier
  provisioner, migrate/seed/admin/public/worker, a mock Walrus publisher enforcing the token claims, the snapshot verified
  with `@mysten/sui`, the console in Chromium against the real admin API) passes; the X manual-assist path needs the X
  definition enabled (documented in docs/PUBLISHING.md).
- 2026-10-30 — R2: released (v0.0.1) and deployed on the cluster; F8 and F17 RESOLVED; the live publisher, the daily export and the egress policy verified. 193 Rust tests, the contract job green against the published client.
- 2026-10-30 — offsite backup live and restore-drilled (in-cluster and from the encrypted R2 set); DB-M9 holds. The drill script now asks for the offsite passphrase once.
- 2026-10-30 — DB-M10: personal-data inventory and a tested erasure path (owner-run SQL, pseudonymises the audit log inside one transaction, verifies, rolls back otherwise). Found: `hub.users.display_name` can hold the e-mail address (the one-time-PIN login sends no name), contrary to the schema comment.
