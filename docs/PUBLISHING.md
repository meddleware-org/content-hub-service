# Publishing: platforms, credentials and going live

How the worker sends posts, what each platform needs from an operator, and how real sends are switched on. The design
is in [ARCHITECTURE.md](ARCHITECTURE.md) §10; this page is the operator's checklist. Nothing here names an operator's
channels: those live in the operator's deployment repository.

## 1. What happens to a post

1. **Enqueue** (admin API): the post is rendered for each target, counted against the target's limits and stored on
   the instance as its **payload** (title, body, SHA-256). Paid platforms reserve their cost under the caps first; one
   that would pass a cap becomes `manual_required` instead. Manual platforms become `manual_required` with an intent
   link.
2. **Claim** (worker): the next due instance is locked (`SKIP LOCKED`). It is stopped (`manual_required`) when its
   platform or target was switched off, deferred when the target reached its posts-per-day limit, recorded as a
   **dry run** when the send would not be live (§3), or set to `sending` and committed **before** the call.
3. **Send**: the stored payload, exactly once.
4. **Finish**: `posted` (with the URL), `failed` (refused: it provably did not go out), `queued` again (provably not
   sent: a refused connection, a 429; bounded attempts with backoff) or `unknown` (it may have gone out). `unknown` is
   never retried: an operator checks the platform and resolves it to `posted` or `failed`.

An edit to a post after it was queued changes nothing already queued. Moving an instance back into `queued`
re-renders and re-validates it from the current post.

## 2. Platforms

| Platform (code) | Target `external_ref` | Target `config` | Credential (`secret_ref` variable holds) | Least privilege |
| --- | --- | --- | --- | --- |
| Owned feed (`OWNED_FEED`) | — | — | none | — |
| Discord (`DISCORD_*`) | — | `mode` (`content` / `embed`), `guild_id` (makes links URLs), `username`, `crosspost_secret_ref` | the channel's **webhook URL** | a webhook on one channel (creating it needs Manage Webhooks once); crossposting needs a bot with Send Messages + Manage Messages in that announcement channel only |
| Telegram (`TELEGRAM_BROADCAST`) | `@channelusername` or the numeric chat id | `disable_link_preview`, `disable_notification` | the **bot token** | a bot added as channel administrator with only "Post messages" |
| Bluesky (`BLUESKY`) | the handle or DID | `pds` (https; default `https://bsky.social`), `langs` | an **app password** (never the account password) | an app password without direct-message access |
| Mastodon (`MASTODON`) | the instance's https base URL | `visibility` (`public` / `unlisted` / `private`), `language` | an **access token** | an application with the `write:statuses` scope only |
| X (`X`, disabled) | — | — | JSON `{"consumer_key", "consumer_secret", "access_token", "access_token_secret"}` | a Read and Write app, no direct messages |

Credential variables are named `PLATFORM_<anything>` (for example `PLATFORM_DISCORD_ANNOUNCEMENTS_WEBHOOK_URL`); the
database refuses any other name, so a target can never point the worker at `DATABASE_URL` or another setting. The
values reach the worker as Kubernetes Secrets (environment variables); they are never stored, logged or returned, and
errors that quote them are scrubbed.

**Manual platforms** (Reddit, directories, forums, X without a budget) are posted by a person from the intent link,
then marked `posted` with the URL. X ships **disabled** (`enabled: false`), so it is not offered at all and a batch that includes an X
target is refused as a whole. To post to X by hand, enable the X definition (the console's platforms, or `PATCH
/api/v1/platforms/{id}` with `{"enabled": true}`): with the spend caps at 0 (the default) every X instance becomes
`manual_required` with its `x.com/intent/tweet` link and the API is never called. **Farcaster** stays an evaluation only: posting goes through a hosted provider
(Neynar) whose free tier is limited, and casts need a signer the hub would have to hold.

Mastodon instance limits are read from `GET /api/v2/instance` by the worker (daily) and cached in the target's
`config.instance_limits`; enqueue validates against them.

## 3. Going live

A send is **live** only when all three hold; otherwise the worker records a dry run (the exact request, credentials
redacted) and hands the instance to a person (`manual_required`):

1. the worker runs with `PUBLISHING_LIVE=1`;
2. the target is marked `live` (admin API; audit-logged; a live target needs `secret_ref`);
3. the process runs in production (`CONTENT_HUB_ENVIRONMENT=production`, the default), or the target is a
   **sandbox** target.

`PUT /api/v1/publishing {"paused": true}` stops every claim at once (sends in progress finish).

**The first real send per platform** goes to a private target owned by the publishing organisation (a private Discord
channel, a private Telegram channel, an `unlisted` Mastodon post, a test account where the platform allows it), with
only that target live, before any public target is switched on. It confirms what mocks cannot:

| Platform | Confirm on the first live send |
| --- | --- |
| Discord | the character unit of the 2000 limit (code points assumed); the message link; crossposting, if used |
| Telegram | the HTML rendering and the link preview; the message link of a private channel |
| Bluesky | that the fixed record key (a TID from the first attempt's time) is accepted; link facets |
| Mastodon | the instance's limits and URL weight; `unlisted` visibility |
| X | (only once funded) the OAuth 1.0a signature and the per-post charge |

## 4. Egress (the worker's allowlist)

The worker connects only to public addresses it resolved itself (no redirects, no proxies, https only). The
namespace's egress policy should allow exactly:

| Host | Why |
| --- | --- |
| `discord.com` | webhooks, crossposting, the optional alert summary |
| `api.telegram.org` | Telegram |
| `bsky.social` (or each target's `pds`) | Bluesky |
| each Mastodon target's instance host | Mastodon |
| `api.x.com` | X, only if enabled |

Plus DNS (kube-dns) and the database.

## 5. Spend and limits

- Every paid send reserves its estimated cost at enqueue under the total, platform and target caps (code default 0;
  the deployment's ceilings; runtime caps at or below them). The worker settles the cost when the post went out and
  releases it when it did not; `unknown` keeps the reservation until an operator resolves it.
- Posts per target per day: 5 by default (`DAILY_POST_LIMIT`, lowered by runtime caps); the rest wait until midnight
  UTC.
- Rate limits: a 429 is retried after the platform's hint (Discord `retry_after`, Telegram `parameters.retry_after`,
  Bluesky `Retry-After`, Mastodon `X-RateLimit-Reset`, X `x-rate-limit-reset`), at most `MAX_ATTEMPTS` (5) times.

## 6. Alerts

With `ALERT_DISCORD_WEBHOOK_URL` set, the worker sends at most one summary per `ALERT_INTERVAL_SECONDS` (default an
hour) to that internal channel: how many instances became `failed` or `unknown`, and spend caps crossing 50, 80 and
100 % this month. Counts only. `/metrics` carries the same numbers for the monitoring stack.

## 7. Before any of this: the publishing organisation

Every platform account is opened by, and held in the name of, the organisation that publishes (not a person). Until
it exists the worker runs in dry-run mode only. Accounts, their credentials and the first live sends are its steps.
