# Owned feed (`OWNED_FEED`)

The hub's own feed (RSS and JSON Feed) and the bot roadmap. It sends nothing anywhere: a post to this target is "posted" when it is stored and published on the feeds host. Part of the publisher runbooks in [README.md](README.md); the flow is in [../PUBLISHING.md](../PUBLISHING.md).

## Credentials

None. The target needs no `secret_ref`; configuration: none.

## Limits and standards

The public routes are read-only (`/public/v1/feed.json`, `/public/v1/feed.xml`, the roadmaps and the signed snapshot); the feed formats are JSON Feed 1.1 and RSS 2.0 and the snapshot is a signed document described in [../SNAPSHOT.md](../SNAPSHOT.md). No third-party service is involved, so there is no external documentation to check against.

## Dry run, live and the stop switch

The owned feed target is created `live` (the end-to-end run does this); an owned-feed send goes nowhere outside the hub, so it is the only target whose live send is harmless. `PUT /api/v1/publishing {"paused": true}` stops every claim, this one included.

## The first live send

The local end-to-end dry run (`integrations/content-hub/` in the instance repository) posts to the owned feed and checks both feed formats and the public roadmap; run it before a release. On the deployed hub, post one item and read the feed host's `feed.json` and `feed.xml`.

## Spend

None.

## Failure modes and alerts

- `failed`: the platform refused the post, so it provably did not go out. Read the attempt record, fix the cause, move the instance back to `queued` to re-render and retry.
- `unknown`: the post may have gone out (a timeout after the request was sent). It is **never** retried. Check the platform yourself, then resolve it to `posted` (with the URL) or `failed`.
- `queued` again: provably not sent (a refused connection, a 429); retried with backoff, at most `MAX_ATTEMPTS` (5) times, waiting for the platform's own hint.
- Alerts (`k8s/clusters/meddleware-org/monitoring/vmalert/rules/alerts.yaml` in the instance repository): `ContentHubSendsNotDelivered` (an instance has been `failed` or `unknown` for an hour),
  `ContentHubWorkerSilent` (the worker's metrics are gone for ten minutes), `ContentHubSpendNearCap` (spend near the monthly cap) and `ContentHubPublishingPausedLong` (the stop switch has been on for a day).
  With `ALERT_DISCORD_WEBHOOK_URL` set the worker also sends one counts-only summary per `ALERT_INTERVAL_SECONDS` to an internal channel.

