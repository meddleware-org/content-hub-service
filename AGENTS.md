# AGENTS.md — content-hub-service

## Purpose

The content hub's backend: posts, roadmap, publishing queue. Read [CLAUDE.md](CLAUDE.md) (invariants) and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (design) before changing anything.

## Layer map

| Layer | Crate | May depend on | Holds |
| --- | --- | --- | --- |
| Pure logic | `content-hub-core` | serde, thiserror, unicode-*, chrono | enumerations, state machines, identifiers, platform definitions, seed validation, the text engine, the delivery decisions, per-platform request and reply rules, the public-address rules |
| Storage | `content-hub-store` | core, sqlx | every SQL statement, migrations, connection settings, test harness |
| Publishing | `content-hub-publish` | core, reqwest, ring | the checked HTTP client; one sender per platform (I/O) |
| Worker | `content-hub-worker` | core, store, publish, api (probes) | claim, send, finish, janitor, metrics, alerts; `snapshot` (the export) |
| HTTP | `content-hub-api` | core, store, axum, reqwest, ring | the two routers, route tables, OpenAPI, probes, Access JWT + Keto + CSRF |
| Binary | `content-hub` (crates/cli) | all | subcommands, configuration from the environment, signals |

## Commands

```bash
bash scripts/test-postgres.sh start && . "${TMPDIR:-/tmp}/content-hub-test-pg/env.sh"
SQLX_OFFLINE=true cargo test --workspace --locked
SQLX_OFFLINE=true cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --check && cargo deny check
bash scripts/tests/check-sbom-test.sh && bash scripts/tests/check-index-test.sh
docker build -t content-hub-service:dev . && bash scripts/smoke.sh content-hub-service:dev
```

## Rules for changes

- A schema change is a new migration plus: regenerated `.sqlx/`, the enumeration lists (core, vectors, client) if a
  CHECK list changed, the public-view column list if a view changed, ARCHITECTURE.md §6.
- A route change is a change to `routes.rs`, the client's `routes.ts` (published first) and ARCHITECTURE.md §3.
- A snapshot-format or signing change regenerates the fixture (`cargo run -p content-hub-core --example
  gen-snapshot-fixture > tests/contract/snapshot-fixture.json`) and must still verify with `@mysten/sui` (contract job).
- A text-engine change regenerates the vectors in the client (`cargo run -p content-hub-core --example gen-vectors`),
  passes there with the TypeScript port, and copies the file here (`core::vectors_tests` compares them).
- Never put an operator's host names, channel ids or plans in this repository.
