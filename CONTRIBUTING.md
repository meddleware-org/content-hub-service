# Contributing to content-hub-service

Thank you for looking. content-hub-service is part of [Meddleware](https://meddleware.co.uk): open tools you can use, take everything when you leave,
and run yourself. This file says how to build it, test it and propose a change.

## Build and test

The tests that touch the database need a PostgreSQL 18 with TLS. `scripts/test-postgres.sh` starts one in a container and writes the settings the tests read.

```bash
bash scripts/test-postgres.sh start                                  # a throwaway PostgreSQL 18 with TLS (Docker, or DOCKER=podman)
. "${TMPDIR:-/tmp}/content-hub-test-pg/env.sh"                       # TEST_DATABASE_URL and REQUIRE_TEST_DATABASE=1
cargo fmt --check                                                    # formatting
SQLX_OFFLINE=true cargo clippy --workspace --all-targets --locked -- -D warnings   # lints; a missing doc comment on any item, private ones included, fails the build
SQLX_OFFLINE=true cargo test --workspace --locked                    # every crate; without TEST_DATABASE_URL the database tests skip, and CI forbids a skip
cargo deny check                                                     # advisories, licences, bans and sources
bash scripts/test-postgres.sh stop
```

After changing a SQL query, regenerate the offline metadata that the compile-time-checked queries use (`.sqlx/`):

```bash
export DATABASE_URL="$(printf '%s' "$TEST_DATABASE_URL" | sed 's#/postgres?#/content_hub_prepare?#')"
sqlx database create && sqlx migrate run --source crates/store/migrations
cargo sqlx prepare --workspace -- --all-targets
```

The README ("Development") has the same commands with their context. CI runs them on every push and pull request (`.github/workflows/rust-ci.yml`), and the release workflow runs them again on the tagged commit.
A change is ready when they all pass locally.

## Proposing a change

1. Open an issue first for anything larger than a fix, so the design can be agreed before the work.
2. Keep a change to one purpose. Write a short subject line in the imperative ("Refuse a Mastodon host on a private address"), and say why in the pull request.
3. **Never edit a migration that has been released.** The migrations are checked by checksum, comments included; a schema change is a new file in `crates/store/migrations/`.
4. **Every SQL statement lives in `crates/store`.** The other crates call its functions, so the database roles (owner, writer, read-only) stay the only way in.
5. Add or update tests with the behaviour they cover. A bug fix comes with a test that failed before it.
6. A change to the snapshot format or the text engine regenerates the fixture or the vectors (see `CLAUDE.md`, "Tests"); the contract tests in `tests/contract` prove the published client still agrees.
7. Add a line to `CHANGELOG.md`.
8. Security problems go to the address in [`SECURITY.md`](SECURITY.md), never to a public issue.

## House rules

- **Licence.** Everything here is [0BSD](LICENSE). By contributing you agree your contribution is released under it.
- **Versions.** Until the platform's mainnet launch, every release is a patch bump, breaking changes included; the changelog says what broke.
- **At most once.** A post is sent to a platform at most once. When the outcome is unknown (a timeout after the request was written), nothing is retried automatically; a person decides.
- **Nothing is sent by default.** Every send is a dry run unless `PUBLISHING_LIVE=1`, and then only to targets marked live.
- **No echo.** A message, log line or error never repeats a configured or caller-supplied URL, token, key or password. Name the key or the position instead. The database URL, the webhook URL and the Telegram token are redacted everywhere they could appear.
- **No secrets, no personal data** in code, tests, fixtures, comments or commit messages. Examples and tests use `.example` hosts and fixed test keys.
- **Privacy.** The service sets no cookie and stores nothing in a visitor's browser. Its one kind of personal data (an operator's identity-provider subject and display name) is inventoried in `docs/PERSONAL_DATA.md`, with a tested erasure path.
- **Open standards.** Where a standard exists and fits, use it; the standards inventory in the audit (`docs/audit/content-hub-service-audit.md`) lists the ones this project speaks.

## Documentation

Every file explains itself, line by line where the code is not self-evident:

- a header on every file saying what it is for;
- a doc comment on every declaration (`///` in Rust, TSDoc in TypeScript, a comment line above a shell function), private ones included;
- comments that say **why**, cite the decision, issue or standard behind a choice, and are updated with the code;
- no `TODO` or `FIXME` without an issue or follow-up reference on the same line;
- every command a document shows has been run, and every link resolves.
