# Discord (`DISCORD_*`)

Posts an announcement to one channel through a webhook, and can crosspost it to the channels that follow an announcement channel. Part of the publisher runbooks in [README.md](README.md); the platform-independent flow is in [../PUBLISHING.md](../PUBLISHING.md).

## Credentials (names only, never values)

| What | Where it lives | Least privilege |
| --- | --- | --- |
| The channel's **webhook URL** | the variable the target's `secret_ref` names, for example `PLATFORM_DISCORD_ANNOUNCEMENTS_WEBHOOK_URL`, in the worker's Kubernetes Secret | a webhook on one channel (creating it needs Manage Webhooks once) |
| Optional: a **bot token** for crossposting | the variable named by the target's `crosspost_secret_ref` | a bot with Send Messages and Manage Messages in that announcement channel only |

The worker accepts only a URL of the form `https://discord.com/api[/v<n>]/webhooks/<id>/<token>` and records it with the token replaced. Target configuration: `mode` (`content` or `embed`), `guild_id` (digits, makes the stored link a message URL), `username`.

## Limits (checked against the official documentation on 2026-11-27)

| Item | Documented | In this hub |
| --- | --- | --- |
| `content` | up to 2000 characters (unit not stated) | 2000, counted in code points; the first live send confirms the unit |
| Embeds | title 256, description 4096, up to 25 fields, field name 256 and value 1024, footer 2048, author name 256; 6000 characters in total across all embeds; up to 10 embeds | `embed` mode sends one embed, title and description (4096) |
| `wait` | `wait=true` returns the created message; the default returns 204 No Content and an unsaved message raises no error | always `wait=true`, so a send is confirmed |
| Mentions | `allowed_mentions` controls notifications; a webhook parses user mentions by default; `{"parse": []}` suppresses all | always `{"parse": []}`: a post can never ping `@everyone`, a role or a user |
| Rate limits | headers `X-RateLimit-*`; a 429 body carries `retry_after` (seconds); 50 requests per second per bot globally; a webhook that returns 404 must not be used again | a 429 is retried after `retry_after`; a 404 is refused |
| Crosspost | `POST /channels/{channel.id}/messages/{message.id}/crosspost`; needs Send Messages for the caller's own messages and Manage Messages for anyone else's | a webhook message is not the bot's own, so the crossposting bot needs Manage Messages in that channel |

The pages read: `docs.discord.com/developers/resources/webhook`, `.../topics/rate-limits` and `.../resources/message` (the old `discord.com/developers` addresses redirect there). None shows a "last updated" date.

## Dry run, live and the stop switch

A send is live only when the worker runs with `PUBLISHING_LIVE=1`, the target is marked `live` (admin API, audit-logged, and it needs a `secret_ref`),
and the process runs in production or the target is a sandbox target (see [../PUBLISHING.md](../PUBLISHING.md) section 3). Otherwise the worker records a dry run
(the exact request with the credential redacted) and hands the instance to a person (`manual_required`). `PUT /api/v1/publishing {"paused": true}` stops every claim
at once; sends already in progress finish.

## The first live send (D85)

1. Create a private channel owned by the publishing organisation and a webhook on it; store the URL with `k8s/scripts/new-content-hub-secrets.sh set PLATFORM_<NAME>` (it prompts and never shows the value).
2. Create a target for it, leave it not live, enqueue a post and read the dry run: the request body, the redacted URL and the character count.
3. Mark only that target live, set `PUBLISHING_LIVE=1`, and let one post go.
4. Confirm what mocks cannot: that the 2000 limit is counted the way the hub counts it, that the stored message link opens the message, and, if crossposting is used, that the message reaches a follower channel.
5. Only then create the public target.

## Spend and the hard stop

Discord is free: its cost is zero. Every paid send reserves its estimated cost at enqueue under the total, platform and target caps (code default 0) and the worker settles it when the post went out; a send that would pass a cap becomes `manual_required` instead. Posts per target per day are limited by `DAILY_POST_LIMIT` (5 by default).

## Failure modes and alerts

- `failed`: the platform refused the post, so it provably did not go out. Read the attempt record, fix the cause, move the instance back to `queued` to re-render and retry.
- `unknown`: the post may have gone out (a timeout after the request was sent). It is **never** retried. Check the platform yourself, then resolve it to `posted` (with the URL) or `failed`.
- `queued` again: provably not sent (a refused connection, a 429); retried with backoff, at most `MAX_ATTEMPTS` (5) times, waiting for the platform's own hint.
- Alerts (`k8s/clusters/meddleware-org/monitoring/vmalert/rules/alerts.yaml` in the instance repository): `ContentHubSendsNotDelivered` (an instance has been `failed` or `unknown` for an hour),
  `ContentHubWorkerSilent` (the worker's metrics are gone for ten minutes), `ContentHubSpendNearCap` (spend near the monthly cap) and `ContentHubPublishingPausedLong` (the stop switch has been on for a day).
  With `ALERT_DISCORD_WEBHOOK_URL` set the worker also sends one counts-only summary per `ALERT_INTERVAL_SECONDS` to an internal channel.

