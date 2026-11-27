# Mastodon (`MASTODON`)

Posts a status to one account on one instance with `POST /api/v1/statuses`. Part of the publisher runbooks in [README.md](README.md); the flow is in [../PUBLISHING.md](../PUBLISHING.md).

## Credentials (names only, never values)

| What | Where it lives | Least privilege |
| --- | --- | --- |
| An **access token** (sent as `Authorization: Bearer <token>`) | the variable the target's `secret_ref` names, for example `PLATFORM_MASTODON_ACCESS_TOKEN`, in the worker's Kubernetes Secret | an application with the `write:statuses` scope only |

The target's `external_ref` is the instance's https base URL; configuration: `visibility` (`public`, `unlisted` or `private`; `direct` is never used) and `language` (ISO 639-1). The worker reads the instance's limits daily and stores them in the target's `instance_limits`; enqueue validates against them.

## Limits (checked against the official documentation on 2026-11-27)

| Item | Documented | In this hub |
| --- | --- | --- |
| `visibility` | `public`, `unlisted`, `private`, `direct` | the first three; an unknown value falls back to `public` |
| Status length | set by each instance (`GET /api/v2/instance`, `configuration.statuses.max_characters`; the sample shows 500) and `characters_reserved_per_url` (the sample shows 23); the pages do not say how URLs and mentions are counted | 500 as the default, replaced by the instance's own values |
| Idempotency | the `Idempotency-Key` header prevents duplicate submissions; keys are stored for up to one hour | a key per instance; a timeout or 5xx is retried with the same key for 45 minutes, then becomes `unknown` |
| Rate limits | 300 requests per 5 minutes per account and per IP; `X-RateLimit-Limit`, `-Remaining` and `-Reset`; the page lists no special limit for creating statuses and does not describe the 429 response (it was last updated 20 November 2022) | a 429 is retried after `X-RateLimit-Reset` |
| Scope | `write:statuses` (also granted by `write`) | `write:statuses` only |

Statuses page: no date shown; the OAuth scopes page: last updated 11 June 2026. The pages do not document `characters_reserved_per_url`, so the first live send confirms the instance's URL weight.

## Dry run, live and the stop switch

A send is live only when the worker runs with `PUBLISHING_LIVE=1`, the target is marked `live` (admin API, audit-logged, and it needs a `secret_ref`),
and the process runs in production or the target is a sandbox target (see [../PUBLISHING.md](../PUBLISHING.md) section 3). Otherwise the worker records a dry run
(the exact request with the credential redacted) and hands the instance to a person (`manual_required`). `PUT /api/v1/publishing {"paused": true}` stops every claim
at once; sends already in progress finish.

## The first live send (D85)

1. Use an account owned by the publishing organisation on a chosen instance. Create an application with the `write:statuses` scope only, authorise it, and store the token with `new-content-hub-secrets.sh set PLATFORM_<NAME>`.
2. Create the target (`external_ref` is the instance URL, `visibility` `unlisted`), keep it not live, let the worker read the instance limits, enqueue, and read the dry run.
3. Mark only that target live, set `PUBLISHING_LIVE=1`, send one post.
4. Confirm the instance's limits and URL weight, and that `unlisted` is honoured. Then switch the target's visibility to the public one it should have.

## Spend and the hard stop

Mastodon is free: its cost is zero. Every paid send reserves its estimated cost at enqueue under the total, platform and target caps (code default 0) and the worker settles it when the post went out; a send that would pass a cap becomes `manual_required` instead. Posts per target per day are limited by `DAILY_POST_LIMIT` (5 by default).

## Failure modes and alerts

- `failed`: the platform refused the post, so it provably did not go out. Read the attempt record, fix the cause, move the instance back to `queued` to re-render and retry.
- `unknown`: the post may have gone out (a timeout after the request was sent). It is **never** retried. Check the platform yourself, then resolve it to `posted` (with the URL) or `failed`.
- `queued` again: provably not sent (a refused connection, a 429); retried with backoff, at most `MAX_ATTEMPTS` (5) times, waiting for the platform's own hint.
- Alerts (`k8s/clusters/meddleware-org/monitoring/vmalert/rules/alerts.yaml` in the instance repository): `ContentHubSendsNotDelivered` (an instance has been `failed` or `unknown` for an hour),
  `ContentHubWorkerSilent` (the worker's metrics are gone for ten minutes), `ContentHubSpendNearCap` (spend near the monthly cap) and `ContentHubPublishingPausedLong` (the stop switch has been on for a day).
  With `ALERT_DISCORD_WEBHOOK_URL` set the worker also sends one counts-only summary per `ALERT_INTERVAL_SECONDS` to an internal channel.

