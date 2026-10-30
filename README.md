# content-hub-service

The backend of the content hub: canonical posts, a public roadmap, and a publishing queue that renders each post per
platform and tracks every publication, automated or manual. One Rust binary with six subcommands, PostgreSQL for
state. Design: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

**Status (0.0.2):** the admin API (Cloudflare Access + Keto, CSRF, cursor pagination, audit log, spend ceilings),
the public API (roadmap for people and for agents, JSON Feed and RSS, OpenAPI, `llms.txt`/`robots.txt`, ETag
caching), migrations, the seed import, and the text engine (rendering, counting, segmentation, intent links) that
validates every payload before it is queued, and the publishing worker (Discord, Telegram, Bluesky, Mastodon, the
owned feed; X built but disabled) with at-most-once sends, dry runs by default and spend caps, and the signed roadmap
snapshot (canonical JSON, a Sui personal-message signature, stored on Walrus). Operators:
[docs/PUBLISHING.md](docs/PUBLISHING.md), [docs/SNAPSHOT.md](docs/SNAPSHOT.md), [docs/PERSONAL_DATA.md](docs/PERSONAL_DATA.md).

## Subcommands

| Subcommand | Runs as | Database role | Status |
| --- | --- | --- | --- |
| `content-hub admin` | Deployment | writer | implemented |
| `content-hub public` | Deployment | read-only | implemented (the feeds list owned-feed posts once the worker exists) |
| `content-hub worker` | Deployment | writer | implemented: dry runs unless `PUBLISHING_LIVE=1` and the target is live |
| `content-hub migrate` | Job before each rollout | owner | implemented |
| `content-hub seed [--path <file>] [--check]` | Job | writer | implemented: the generic definitions, plus the instance seed (YAML, or JSON for `.json`) when given; `--check` validates without writing |
| `content-hub export-snapshot` | CronJob | writer | implemented: signs and stores a snapshot ([docs/SNAPSHOT.md](docs/SNAPSHOT.md)) |
| `content-hub snapshot-key --out <path>` / `snapshot-address` | on the node, once | — | generates the signer's seed privately and prints only the address / prints the address of `SNAPSHOT_SIGNER_SEED` |

## Configuration (environment only)

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | — (required) | `postgres://user:password@host:5432/content_hub?sslmode=verify-full&sslrootcert=/etc/postgres-ca/ca.crt`. Any other `sslmode` is refused |
| `ALLOW_INSECURE_DATABASE` | unset | `1` accepts a plaintext server (local tests only) |
| `LISTEN_ADDR` | `0.0.0.0:8080` | `admin` and `public` |
| `DB_POOL_SIZE` | `8` | 1 to 64 |
| `RUST_LOG` | `info` | log filter; logs are JSON on stdout |

`admin` also needs:

| Variable | Default | Meaning |
| --- | --- | --- |
| `ACCESS_TEAM_DOMAIN` | — (required) | `https://<team>.cloudflareaccess.com`: the JWT issuer; the JWKS is `{team}/cdn-cgi/access/certs` |
| `ACCESS_AUD` | — (required) | the Access application's AUD tag |
| `ADMIN_ORIGIN` | — (required) | `https://` origin of the admin UI; state-changing requests must carry it as `Origin` |
| `KETO_READ_URL` | — (required in production) | Keto's read API; the role is `ContentHub:admin#member@<sub>` |
| `SPEND_CEILING_TOTAL_USD_MICROS` | `0` | the instance ceiling for all external spend per month; runtime caps cannot exceed it |
| `SPEND_CEILING_PER_PLATFORM_USD_MICROS` | `0` | the instance ceiling per platform per month |
| `CONTENT_HUB_ENVIRONMENT` | `production` | `development` enables `ACCESS_JWKS_FILE` (a static JWKS) and `CONTENT_HUB_DEV_ADMINS` (comma-separated subjects instead of Keto); production refuses both |

`worker` reads:

| Variable | Default | Meaning |
| --- | --- | --- |
| `PUBLISHING_LIVE` | `0` | `1` allows real sends, to targets marked `live` only (outside production: sandbox targets only) |
| `CONTENT_HUB_ENVIRONMENT` | `production` | `development`: only sandbox targets can be sent to |
| `PLATFORM_*` | — | the credentials the targets name (`secret_ref`); see docs/PUBLISHING.md |
| `POLL_INTERVAL_MS` | `5000` | pause when nothing is due (500 to 60000) |
| `CLAIM_LEASE_SECONDS` | `600` | a `sending` row older than this becomes `unknown` (120 to 86400) |
| `MAX_ATTEMPTS` | `5` | sends per instance when each was provably not delivered (1 to 10) |
| `DAILY_POST_LIMIT` | `5` | posts per target per day, the ceiling of the runtime caps (1 to 1000) |
| `SPEND_CEILING_TOTAL_USD_MICROS`, `SPEND_CEILING_PER_PLATFORM_USD_MICROS` | `0` | the same ceilings as `admin` (alert levels, metrics) |
| `ALERT_DISCORD_WEBHOOK_URL` | unset | an internal Discord channel for the optional summary of failures and spend levels |
| `ALERT_INTERVAL_SECONDS` | `3600` | at most one summary per interval (300 to 86400) |
| `WATCH_WALLETS` | unset | `label=0xaddress,…` (at most ten public addresses): their SUI and WAL balances are checked and exposed on `/metrics` (docs/MONITORING.md) |
| `SUI_GRAPHQL_URL` | — (required with `WATCH_WALLETS`) | a Sui GraphQL endpoint, https, e.g. `https://graphql.testnet.sui.io/graphql` |
| `WATCH_INTERVAL_SECONDS` | `300` | how often the balances are checked (60 to 3600) |

`export-snapshot` reads `SNAPSHOT_SIGNER_SEED` (required), `SNAPSHOT_NETWORK` (default `testnet`; mainnet needs
`SNAPSHOT_ALLOW_MAINNET=1`), `SNAPSHOT_EPOCHS`, `SNAPSHOT_MIN_INTERVAL_HOURS`, `SNAPSHOT_RENEW_MARGIN_DAYS`,
`WALRUS_PUBLISHER_URL` and `WALRUS_PUBLISHER_JWT_SECRET`; the full table is in [docs/SNAPSHOT.md](docs/SNAPSHOT.md).

`public` also needs:

| Variable | Default | Meaning |
| --- | --- | --- |
| `HOME_PAGE_URL` | — (required) | `https://` URL of the human roadmap site (the feeds link to it) |
| `FEEDS_BASE_URL` | — (required) | `https://` base URL the feeds are served from (their self links) |
| `FEED_TITLE` | `Updates` | title of both feeds |
| `LLMS_TXT_FILE`, `ROBOTS_TXT_FILE` | built-in | files (ConfigMap mounts) served at `/llms.txt` and `/robots.txt` |
| `WALRUS_AGGREGATOR_URL` | unset | a public Walrus aggregator (https), so `/public/v1/snapshot` can link the stored blob |

## Database roles

Three roles, provisioned before the first `migrate` (on the platform's database tier by its `owner`, `writer` and
`readonly` records; ARCHITECTURE.md §6). On another PostgreSQL, as a superuser (names are examples):

```sql
CREATE ROLE content_hub_migrate LOGIN PASSWORD '…';
CREATE ROLE content_hub_app     LOGIN PASSWORD '…';
CREATE ROLE content_hub_public  LOGIN PASSWORD '…';
ALTER ROLE content_hub_public SET default_transaction_read_only = on;
CREATE DATABASE content_hub OWNER content_hub_migrate;
REVOKE ALL ON DATABASE content_hub FROM PUBLIC;
GRANT CONNECT, TEMPORARY, CREATE ON DATABASE content_hub TO content_hub_migrate;
GRANT CONNECT ON DATABASE content_hub TO content_hub_app, content_hub_public;
\c content_hub
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT USAGE, CREATE ON SCHEMA public TO content_hub_migrate;
GRANT USAGE ON SCHEMA public TO content_hub_app, content_hub_public;
ALTER DEFAULT PRIVILEGES FOR ROLE content_hub_migrate IN SCHEMA public GRANT SELECT ON TABLES TO content_hub_app, content_hub_public;
CREATE SCHEMA hub AUTHORIZATION content_hub_migrate;
GRANT USAGE ON SCHEMA hub TO content_hub_app;
ALTER DEFAULT PRIVILEGES FOR ROLE content_hub_migrate IN SCHEMA hub
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO content_hub_app;
```

## Development

```bash
bash scripts/test-postgres.sh start            # PostgreSQL 18 with TLS (DOCKER=podman works)
. "${TMPDIR:-/tmp}/content-hub-test-pg/env.sh"  # TEST_DATABASE_URL, REQUIRE_TEST_DATABASE=1
SQLX_OFFLINE=true cargo test --workspace --locked
SQLX_OFFLINE=true cargo clippy --workspace --all-targets --locked -- -D warnings
cargo deny check
bash scripts/test-postgres.sh stop
```

Queries are checked at compile time against `.sqlx/` (committed). After changing a query or a migration, regenerate
it against a migrated database:

```bash
export DATABASE_URL="$(printf '%s' "$TEST_DATABASE_URL" | sed 's#/postgres?#/content_hub_prepare?#')"
sqlx database create && sqlx migrate run --source crates/store/migrations
cargo sqlx prepare --workspace -- --all-targets
```

Image: `docker build -t content-hub-service:dev .`, then `bash scripts/smoke.sh content-hub-service:dev`.

## Repository layout

```text
crates/core       pure logic: enumerations, state machines, identifiers, platform definitions, seed validation,
                  the text engine (markup, renderers, counters, segmentation, intent links) (no I/O)
crates/store      PostgreSQL: settings, migrations (migrations/), the test harness; all SQL lives here
crates/publish    platform publishers: the checked HTTP client, one sender per platform, the Walrus publisher client (`BlobStore`)
crates/worker     the publishing worker: claim, send, finish, janitor, metrics, alerts
crates/api        the admin and public routers, route tables, OpenAPI, Access/Keto/CSRF middleware
crates/cli        the `content-hub` binary
seed/             the generic platform definitions (embedded)
schema/           the public JSON Schema of an instance roadmap seed
vectors.json      copy of @meddleware/content-hub-client's golden vectors (checked against the published file;
                  regenerate with `cargo run -p content-hub-core --example gen-vectors`)
```

## Licence

0BSD — see [LICENSE](LICENSE).
