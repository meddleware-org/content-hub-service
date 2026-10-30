# Changelog

## 0.0.1 — unreleased

- Schema version 1 (`crates/store/migrations/0001_init.sql`): 18 tables in schema `hub`, four public views in
  `public`, append-only audit log and spend ledger.
- `content-hub migrate`: embedded, checksummed, forward-only migrations under an advisory lock; a newer schema is
  refused.
- `content-hub admin`: the admin API — phases, items, tasks (state machine, dependencies, platforms, links), posts and
  revisions, enqueue (rendered and counted per platform; a platform with a non-zero cost is refused until the worker reserves
  spend), instances (mark posted,
  cancel, resolve unknown, retry), platform definitions and targets, budgets, the schedule, stats, the audit log;
  Cloudflare Access JWT verification (ring RS256, cached JWKS), Keto roles (fail closed), CSRF; keyset pagination;
  1 MiB / 30 s limits; URLs must be `https` to a public host (private, loopback, link-local and internal names
  refused).
- `content-hub public`: `/public/v1/roadmap`, `/public/v1/roadmap-bot`, `/public/v1/feed.json` (JSON Feed 1.1),
  `/public/v1/feed.xml` (RSS 2.0), `/public/v1/openapi.json`, `/llms.txt`, `/robots.txt`; ETag and Cache-Control;
  16 KiB / 10 s limits.
- `content-hub seed [--path] [--check]`: the generic definitions and an instance seed (YAML or JSON) upserted in one
  transaction, idempotent; an operator's edits to a definition's limits and enabled flag survive a re-seed.
- Core: the text engine — a CommonMark subset, five renderers with offset maps, six counters (including X's weighted
  count and Mastodon's URL and mention rules), segmentation for the composer and per-platform limit checks, intent
  links with RFC 3986 encoding — and the counting, rendering and segmentation vectors (`examples/gen-vectors.rs`).
- `content-hub worker`: claims due instances (`SKIP LOCKED`), commits `sending` before every call, sends the stored
  payload once, records every attempt, retries only what provably did not go out (bounded, backoff with jitter), never
  retries `unknown`; janitor; dry runs unless `PUBLISHING_LIVE=1` and the target is live (outside production only
  sandbox targets); the `publishing.paused` stop switch; posts per target per day; `/metrics`; an optional alert
  summary; daily Mastodon instance limits. Publishers: Discord webhooks (mentions never parsed; optional crosspost),
  Telegram, Bluesky (link facets, fixed record keys), Mastodon (Idempotency-Key), the owned feed; X (OAuth 1.0a)
  built but disabled.
- Enqueue stores the validated payload (title, body, SHA-256, post revision) on each instance, reserves paid sends'
  cost under the caps (over a cap: `manual_required`), and every operator re-queue re-validates and re-reserves;
  cancelling releases, confirming an `unknown` as posted settles.
- `GET /api/v1/preview/roadmap?audience=human|bot` (the public document itself, for the console's preview); stats gain
  `posts_by_phase`; `GET`/`PUT /api/v1/publishing` (the stop switch); targets gain `live`; credential names must start with `PLATFORM_`;
  Mastodon instance URLs and Bluesky `pds` must be https on a public host.
- `content-hub export-snapshot`, `snapshot-key`, `snapshot-address` and `GET /public/v1/snapshot`: the canonical JSON
  snapshot of both public roadmaps, signed as a Sui personal message (verified against `@mysten/sui` in CI), stored on
  Walrus through the internal publisher (HS256 tokens bound to the exact epochs and size), recorded with its hash,
  signature, signer, blob and estimated expiry; at most one new snapshot a day, renewed before storage ends, mainnet off
  unless allowed. The Walrus client for the publisher may call only its configured host.
- docs/PERSONAL_DATA.md and scripts/erase-person.sql: what is stored about signed-in people, and a tested erasure path (smoke step 9).
- docs/SNAPSHOT.md: the format, the signature, verification and operation.
- docs/PUBLISHING.md: platforms, credentials, least-privilege accounts, egress, going live.
- Roadmap items have an optional public due date (`public_due_at`, exposed as `due_at` in the public views).
- Core: enumerations, task and platform-instance state machines, identifier shapes, the 38 generic platform
  definitions (checked against each platform's documentation on 2026-10-25).
- The public JSON Schema of an instance roadmap seed (`schema/roadmap-seed.schema.json`).
