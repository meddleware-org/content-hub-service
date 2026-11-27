# Telegram (`TELEGRAM_BROADCAST`)

Posts to one channel with the Bot API `sendMessage`. Part of the publisher runbooks in [README.md](README.md); the flow is in [../PUBLISHING.md](../PUBLISHING.md).

## Credentials (names only, never values)

| What | Where it lives | Least privilege |
| --- | --- | --- |
| The **bot token** (`<bot id>:<secret>`) | the variable the target's `secret_ref` names, for example `PLATFORM_TELEGRAM_BOT_TOKEN`, in the worker's Kubernetes Secret | a bot added to the channel as an administrator with only "Post messages" (`can_post_messages`) |

The token is a path segment of the request URL, so the hub stores and logs only a redacted URL. The target's `external_ref` is `@channelusername` or the numeric chat id; configuration: `disable_link_preview`, `disable_notification`.

## Limits (checked against the official documentation on 2026-11-27; Bot API 10.5, 22 November 2026)

| Item | Documented | In this hub |
| --- | --- | --- |
| Text | 1 to 4096 characters after entities are parsed (the unit is not stated) | 4096, counted in UTF-16 units over the rendered text; the first live send confirms it |
| HTML mode | `b`/`strong`, `i`/`em`, `u`/`ins`, `s`/`strike`/`del`, `span class="tg-spoiler"`, `a href`, `code`, `pre`, `blockquote` and a few more; `<`, `>` and `&` must be escaped; named entities `&lt; &gt; &amp; &quot;` only; `pre`/`code` cannot contain other entities | the text engine renders and escapes the post as Telegram HTML |
| Chat | `chat_id` is an integer or a string `@username` | both accepted |
| Silent send | `disable_notification` sends without sound; link previews are set by `link_preview_options` (there is no `disable_web_page_preview` parameter) | `disable_link_preview` maps to the link-preview options |
| Rate limits | about one message per second per chat; 20 messages per minute in a group; about 30 messages per second for a broadcast; a 429 carries `parameters.retry_after` (seconds) | a 429 is retried after `retry_after` |

The Bot API page and the bots FAQ disagree on the Stars threshold for paid broadcasts (10,000 on one, 100,000 on the other); the hub does not use paid broadcasts.

## Dry run, live and the stop switch

A send is live only when the worker runs with `PUBLISHING_LIVE=1`, the target is marked `live` (admin API, audit-logged, and it needs a `secret_ref`),
and the process runs in production or the target is a sandbox target (see [../PUBLISHING.md](../PUBLISHING.md) section 3). Otherwise the worker records a dry run
(the exact request with the credential redacted) and hands the instance to a person (`manual_required`). `PUT /api/v1/publishing {"paused": true}` stops every claim
at once; sends already in progress finish.

## The first live send (D85)

1. Create a private channel owned by the publishing organisation, make the bot an administrator with only "Post messages", and store the token with `new-content-hub-secrets.sh set PLATFORM_<NAME>`.
2. Create the target (`external_ref` is the private channel's `@username` or numeric id), keep it not live, enqueue, and read the dry run.
3. Mark only that target live, set `PUBLISHING_LIVE=1`, send one post.
4. Confirm the HTML rendering, the link preview and the message link of a private channel; only then create the public target.

## Spend and the hard stop

Telegram is free: its cost is zero. Every paid send reserves its estimated cost at enqueue under the total, platform and target caps (code default 0) and the worker settles it when the post went out; a send that would pass a cap becomes `manual_required` instead. Posts per target per day are limited by `DAILY_POST_LIMIT` (5 by default).

## Failure modes and alerts

- `failed`: the platform refused the post, so it provably did not go out. Read the attempt record, fix the cause, move the instance back to `queued` to re-render and retry.
- `unknown`: the post may have gone out (a timeout after the request was sent). It is **never** retried. Check the platform yourself, then resolve it to `posted` (with the URL) or `failed`.
- `queued` again: provably not sent (a refused connection, a 429); retried with backoff, at most `MAX_ATTEMPTS` (5) times, waiting for the platform's own hint.
- Alerts (`k8s/clusters/meddleware-org/monitoring/vmalert/rules/alerts.yaml` in the instance repository): `ContentHubSendsNotDelivered` (an instance has been `failed` or `unknown` for an hour),
  `ContentHubWorkerSilent` (the worker's metrics are gone for ten minutes), `ContentHubSpendNearCap` (spend near the monthly cap) and `ContentHubPublishingPausedLong` (the stop switch has been on for a day).
  With `ALERT_DISCORD_WEBHOOK_URL` set the worker also sends one counts-only summary per `ALERT_INTERVAL_SECONDS` to an internal channel.

