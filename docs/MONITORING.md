# Monitoring the content hub

The worker's `/metrics` (port 8080, in-cluster only) is the one scrape target; the numbers come from the database at scrape
time, so every replica reports the same values. This page lists them and the alerts worth making. The alert rules belong to
the operator's monitoring stack (an instance repository), because thresholds and receivers are instance policy.

| Metric | Type | Meaning | Alert on |
| --- | --- | --- | --- |
| `content_hub_instances{status}` | gauge | platform instances by status | `failed` or `unknown` above 0 for a long time; `queued` growing |
| `content_hub_delivery_attempts_total{platform,outcome}` | counter | finished attempts | a rising failure rate on one platform |
| `content_hub_spend_used_usd_micros{platform}` / `content_hub_spend_cap_usd_micros{scope,platform}` | gauge | this month's spend against the cap in force | used / cap ≥ 0.5, 0.8, 1 |
| `content_hub_publishing_paused` | gauge | 1 while the stop switch is on | 1 for longer than a deliberate pause |
| `content_hub_worker_live` | gauge | 1 when this worker may send for real | unexpected 1 |
| `content_hub_snapshot_generated_timestamp_seconds{network}` | gauge | when the latest signed snapshot was generated (moves only when the roadmap changes) | (informational) |
| `content_hub_snapshot_storage_expires_timestamp_seconds{network}` | gauge | when that snapshot's Walrus storage is estimated to end; the export renews it a week before | less than 3 days left = the renewals are failing (publisher down, float spent) |
| `content_hub_wallet_balance{wallet,address,coin}` | gauge | a watched wallet's balance in MIST (SUI) or FROST (WAL), from the last successful check; always reported for SUI and WAL (0 when none) | SUI or WAL below a few stores' worth |
| `content_hub_wallet_checked_timestamp_seconds{wallet}` | gauge | unix time of the last **successful** check (0: never) | older than 30 minutes (a dead endpoint must not look like a stable balance) |
| `content_hub_wallet_check_failures_total{wallet}` | counter | failed checks since the process started | rising |

## The wallet watch

`WATCH_WALLETS=publisher=0x…` (public addresses only; no key is ever involved) makes the worker poll `SUI_GRAPHQL_URL` every
`WATCH_INTERVAL_SECONDS` in one background task, not once per scrape, and keep the last good reading. The checked HTTP client
is used: https only, public addresses only, no redirects. The namespace's egress policy must allow the GraphQL host.

Why it exists: the internal Walrus publisher stores the daily snapshot from its own small float (ADR-0007). A depleted float
used to show only as a failed daily export; the balance gauges let the operator be told days earlier, and the
snapshot-expiry gauge catches any other reason renewals stop.
