# Publisher runbooks

One page for each platform the content hub can send to: its credentials (names only), its limits and rate limits as the platform documents them, how a send becomes live, the spend rules and the first live send (decision D85), and the failure modes. The platform-independent flow (enqueue, claim, send, finish, the stop switch, egress) is in [../PUBLISHING.md](../PUBLISHING.md); the design is in [../ARCHITECTURE.md](../ARCHITECTURE.md).

| Platform | Runbook | Cost | Limits checked against the platform's documentation |
| --- | --- | --- | --- |
| Owned feed | [owned-feed.md](owned-feed.md) | none | not applicable (no third party) |
| Discord | [discord.md](discord.md) | none | 2026-11-27 |
| Telegram | [telegram.md](telegram.md) | none | 2026-11-27 (Bot API 10.5) |
| Bluesky | [bluesky.md](bluesky.md) | none | 2026-11-27 |
| Mastodon | [mastodon.md](mastodon.md) | none | 2026-11-27 |
| X (disabled) | [x.md](x.md) | 0.015 US dollars a post, 0.20 with a URL | 2026-11-27 |

## Rules for every platform

- **No live sends from tests or from this documentation.** Automated tests run against mocks built from each platform's documented API (D85); the first real send per platform goes to a private target owned by the publishing organisation.
- **Credentials are never written down.** A target names a variable (`PLATFORM_<NAME>`); the value reaches the worker as a Kubernetes Secret, created with `k8s/scripts/new-content-hub-secrets.sh` in the instance repository, which prompts for it and never shows it.
- **Check the numbers again when a platform changes.** The "checked" dates above are when the limits were last read against the official pages; each page lists the pages read and where one was out of reach or silent. The seeded limits live in `crates/core/src/platforms.rs`.
