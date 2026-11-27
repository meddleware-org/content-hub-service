# Bluesky (`BLUESKY`)

Posts to one account with the AT Protocol: `com.atproto.server.createSession` with an app password, then `com.atproto.repo.createRecord` of an `app.bsky.feed.post`. Part of the publisher runbooks in [README.md](README.md); the flow is in [../PUBLISHING.md](../PUBLISHING.md).

## Credentials (names only, never values)

| What | Where it lives | Least privilege |
| --- | --- | --- |
| An **app password** (never the account password) | the variable the target's `secret_ref` names, for example `PLATFORM_BLUESKY_APP_PASSWORD`, in the worker's Kubernetes Secret | an app password without direct-message access |

The target's `external_ref` is the account handle or DID; configuration: `pds` (an https base URL, default `https://bsky.social`) and `langs` (up to 3 language tags).

## Limits (checked on 2026-11-27 against the lexicons in the `bluesky-social/atproto` repository and the Bluesky rate-limit guide)

| Item | Documented | In this hub |
| --- | --- | --- |
| Post `text` | `maxGraphemes` 300 and `maxLength` 3000 (bytes) | 300 graphemes; the byte limit is checked too |
| `langs` | at most 3 language tags | at most 3 |
| Links | richtext facets; `byteStart` is inclusive and `byteEnd` exclusive, counted in **UTF-8 bytes** | one link facet per bare URL, with byte offsets |
| Record key | `app.bsky.feed.post` uses the `tid` key type; a key is 1 to 512 characters of `A-Za-z0-9.-_:~` | a TID built from the first attempt's time and the instance's idempotency key, so every attempt writes the same record |
| createRecord | `repo`, `collection`, `record` required; `rkey` optional (max 512); `validate` optional | all four used |
| Writes (per account) | 5,000 points per hour and 35,000 per day; a create costs 3 points (about 1,666 creates an hour) | far below the limit at 5 posts a day |
| createSession (per account) | 30 per 5 minutes and 300 per day | one session per attempt: at most a handful a day |
| Other limits | 3,000 requests per 5 minutes per IP; blob uploads 50 MB | not reached |

Several pages of the old `docs.bsky.app` site now redirect or return empty bodies, so the numbers come from the lexicon JSON (`packages/pds/lexicons/...`) and the rate-limit guide in the `bsky-docs` repository. Re-read them when the SDK or the service changes.

## Dry run, live and the stop switch

A send is live only when the worker runs with `PUBLISHING_LIVE=1`, the target is marked `live` (admin API, audit-logged, and it needs a `secret_ref`),
and the process runs in production or the target is a sandbox target (see [../PUBLISHING.md](../PUBLISHING.md) section 3). Otherwise the worker records a dry run
(the exact request with the credential redacted) and hands the instance to a person (`manual_required`). `PUT /api/v1/publishing {"paused": true}` stops every claim
at once; sends already in progress finish.

## The first live send (D85)

1. Use an account owned by the publishing organisation (a test account where one exists), create an app password for it, and store it with `new-content-hub-secrets.sh set PLATFORM_<NAME>`.
2. Create the target (`external_ref` is the handle), keep it not live, enqueue, and read the dry run: the facets and the planned record key.
3. Mark only that target live, set `PUBLISHING_LIVE=1`, send one post.
4. Confirm that the fixed record key (a TID from the first attempt's time) is accepted, and that the link facets are clickable. The URL of the post is known before the call (it is in the attempt record), which is how an `unknown` is resolved: open it.

## Spend and the hard stop

Bluesky is free: its cost is zero. Every paid send reserves its estimated cost at enqueue under the total, platform and target caps (code default 0) and the worker settles it when the post went out; a send that would pass a cap becomes `manual_required` instead. Posts per target per day are limited by `DAILY_POST_LIMIT` (5 by default).

## Failure modes and alerts

- `failed`: the platform refused the post, so it provably did not go out. Read the attempt record, fix the cause, move the instance back to `queued` to re-render and retry.
- `unknown`: the post may have gone out (a timeout after the request was sent). It is **never** retried. Check the platform yourself, then resolve it to `posted` (with the URL) or `failed`.
- `queued` again: provably not sent (a refused connection, a 429); retried with backoff, at most `MAX_ATTEMPTS` (5) times, waiting for the platform's own hint.
- Alerts (`k8s/clusters/meddleware-org/monitoring/vmalert/rules/alerts.yaml` in the instance repository): `ContentHubSendsNotDelivered` (an instance has been `failed` or `unknown` for an hour),
  `ContentHubWorkerSilent` (the worker's metrics are gone for ten minutes), `ContentHubSpendNearCap` (spend near the monthly cap) and `ContentHubPublishingPausedLong` (the stop switch has been on for a day).
  With `ALERT_DISCORD_WEBHOOK_URL` set the worker also sends one counts-only summary per `ALERT_INTERVAL_SECONDS` to an internal channel.

