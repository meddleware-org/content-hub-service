# X (`X`, ships disabled)

The definition ships **disabled**. Posting through the API is pay-per-use, so with the spend caps at their default of 0 every X instance becomes `manual_required` with an `x.com/intent/tweet` link and the API is never called. Part of the publisher runbooks in [README.md](README.md); the flow is in [../PUBLISHING.md](../PUBLISHING.md).

## Credentials (names only, never values)

| What | Where it lives | Least privilege |
| --- | --- | --- |
| A JSON credential `{"consumer_key", "consumer_secret", "access_token", "access_token_secret"}` (OAuth 1.0a user context) | the variable the target's `secret_ref` names, in the worker's Kubernetes Secret | a Read and Write application with no direct-message access |

OAuth 2.0 with PKCE is deliberately not implemented: its access tokens expire and each refresh issues a new refresh token, which the hub would have to store, and the hub never stores credentials.

## Limits and prices (checked against the official documentation on 2026-11-27; no date is shown on the pages)

| Item | Documented | In this hub |
| --- | --- | --- |
| Endpoint | `POST https://api.x.com/2/tweets`, JSON body, success is 201; `text` is "at most 280 characters unless the account has X Premium" | 280, counted with the weighted rules; the pages read do not describe weighting or how URLs count, so the first funded send confirms it |
| Price | pay-per-use: "Post: Create" 0.015 US dollars, "Post: Create (with URL)" 0.200; a spending limit can be set per billing cycle; "Prices are subject to change" | the caps reserve these amounts; posts with a link cost the URL price |
| Rate limits | creating a post: 100 per user per 15 minutes and 10,000 per app per 24 hours; a 429 body is `{"errors":[{"code":88,...}]}`; headers `x-rate-limit-limit`, `-remaining` and `-reset` (a Unix time) | a 429 is retried after `x-rate-limit-reset` |
| Authentication | OAuth 2.0 user token with `tweet.write`, or a user token (OAuth 1.0a); the signature is HMAC-SHA1 over the method, base URL and sorted, percent-encoded parameters | OAuth 1.0a; the pages read do not say whether a JSON body enters the signature base string (RFC 5849, the OAuth 1.0a specification, includes only form-encoded bodies), so confirm on the first funded send |
| Quote posts | need an Enterprise plan | not used |
| Web intent | `x.com/intent/tweet` is not documented on the pages read | the free route for every post: the hub builds the link and a person posts |

## Dry run, live and the stop switch

A send is live only when the worker runs with `PUBLISHING_LIVE=1`, the target is marked `live` (admin API, audit-logged, and it needs a `secret_ref`),
and the process runs in production or the target is a sandbox target (see [../PUBLISHING.md](../PUBLISHING.md) section 3). Otherwise the worker records a dry run
(the exact request with the credential redacted) and hands the instance to a person (`manual_required`). `PUT /api/v1/publishing {"paused": true}` stops every claim
at once; sends already in progress finish.

## The first live send (D85)

Only once the account is funded and the caps are raised above zero, and only to a private or test account:

1. Fund the developer account with a small amount and set the platform's spending limit to a few cents.
2. Raise the hub's X caps just enough for one post, store the credential with `new-content-hub-secrets.sh set PLATFORM_<NAME>`, enable the X definition, mark the target live.
3. Send one post without a link and check the charge; then one with a link and check the URL price.
4. Confirm the OAuth 1.0a signature is accepted and the per-post charge matches the pages. Set the caps back to 0 afterwards if nothing more is planned.

## Spend and the hard stop

X is the only platform that costs money: 0.015 US dollars a post, 0.20 with a URL. Every paid send reserves its estimated cost at enqueue under the total, platform and target caps (code default 0) and the worker settles it when the post went out; a send that would pass a cap becomes `manual_required` instead. Posts per target per day are limited by `DAILY_POST_LIMIT` (5 by default).

## Failure modes and alerts

- `failed`: the platform refused the post, so it provably did not go out. Read the attempt record, fix the cause, move the instance back to `queued` to re-render and retry.
- `unknown`: the post may have gone out (a timeout after the request was sent). It is **never** retried. Check the platform yourself, then resolve it to `posted` (with the URL) or `failed`.
- `queued` again: provably not sent (a refused connection, a 429); retried with backoff, at most `MAX_ATTEMPTS` (5) times, waiting for the platform's own hint.
- Alerts (`k8s/clusters/meddleware-org/monitoring/vmalert/rules/alerts.yaml` in the instance repository): `ContentHubSendsNotDelivered` (an instance has been `failed` or `unknown` for an hour),
  `ContentHubWorkerSilent` (the worker's metrics are gone for ten minutes), `ContentHubSpendNearCap` (spend near the monthly cap) and `ContentHubPublishingPausedLong` (the stop switch has been on for a day).
  With `ALERT_DISCORD_WEBHOOK_URL` set the worker also sends one counts-only summary per `ALERT_INTERVAL_SECONDS` to an internal channel.

