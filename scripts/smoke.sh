#!/usr/bin/env bash
# Smoke test of a built content-hub-service image: bash scripts/smoke.sh <image>   (DOCKER=podman to use podman)
#
# Runs the image the way the cluster does — each subcommand as its own container, non-root, read-only root
# filesystem, no capabilities, each with its own database role — against a real PostgreSQL 18 with TLS
# (scripts/test-postgres.sh) provisioned like the platform's database tier (owner, writer on schema `hub`,
# read-only on `public`). It checks:
#   - the image runs as the distroless nonroot user (65532) and its binary answers --version;
#   - before the migrate Job ran, `public` and `admin` answer /healthz 200 and /readyz 503 (schema behind);
#   - `migrate` as the owner exits 0, twice (idempotent);
#   - afterwards both answer /readyz 200; `public` serves its routes and 404s every admin route; `admin` 404s
#     the public routes and refuses its own without an Access token (401);
#   - `migrate` with an application role's credentials fails (no DDL privilege);
#   - `seed` (the generic platform definitions) as the writer exits 0, twice; as the read-only role it fails;
#   - `worker` answers its probes (not ready before the migration), serves /metrics (dry-run by default:
#     content_hub_worker_live 0), and refuses an invalid PUBLISHING_LIVE;
#   - `snapshot-key` writes a new private seed file and prints only the address; `export-snapshot` with a signer and
#     no publisher signs and records a snapshot (twice: the second is "unchanged") and `public` serves it at
#     /public/v1/snapshot with the same signer address; a bad seed or an unallowed mainnet export exits non-zero.
set -euo pipefail

image="${1:?usage: smoke.sh <image>}"
docker="${DOCKER:-docker}"
here="$(cd "$(dirname "$0")" && pwd)"
public_port=18081
admin_port=18082
worker_port=18083
pgport=55443
pgdir="$(mktemp -d)"
pgname="content-hub-smoke-pg"

user="$("$docker" image inspect --format '{{.Config.User}}' "$image")"
if [ "$user" != "65532:65532" ]; then
  echo "image runs as '${user}', want 65532:65532" >&2
  exit 1
fi

public="" admin="" worker=""
cleanup() {
  for c in "$public" "$admin" "$worker"; do
    [ -n "$c" ] || continue
    echo "--- logs of ${c:0:12}" >&2
    "$docker" logs "$c" 2>&1 | tail -20 >&2 || true
    "$docker" rm -f "$c" > /dev/null 2>&1 || true
  done
  DOCKER="$docker" PG_NAME="$pgname" PG_DIR="$pgdir" bash "$here/test-postgres.sh" stop > /dev/null 2>&1 || true
}
trap cleanup EXIT

# 1. PostgreSQL with TLS, and the service's database provisioned as the tier does it.
DOCKER="$docker" PG_NAME="$pgname" PG_PORT="$pgport" PG_DIR="$pgdir" bash "$here/test-postgres.sh" start > /dev/null
own_pw="$(openssl rand -hex 16)"
app_pw="$(openssl rand -hex 16)"
pub_pw="$(openssl rand -hex 16)"
psql_admin() { "$docker" exec -i "$pgname" psql -X -q -v ON_ERROR_STOP=1 -U postgres -h 127.0.0.1 "$@"; }
psql_admin -d postgres << SQL
CREATE ROLE content_hub_migrate LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION CONNECTION LIMIT 2 PASSWORD '${own_pw}';
CREATE ROLE content_hub_app LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION CONNECTION LIMIT 12 PASSWORD '${app_pw}';
CREATE ROLE content_hub_public LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION CONNECTION LIMIT 10 PASSWORD '${pub_pw}';
ALTER ROLE content_hub_app SET statement_timeout = '30s';
ALTER ROLE content_hub_public SET default_transaction_read_only = on;
ALTER ROLE content_hub_public SET statement_timeout = '5s';
CREATE DATABASE content_hub OWNER content_hub_migrate;
REVOKE ALL ON DATABASE content_hub FROM PUBLIC;
GRANT CONNECT, TEMPORARY, CREATE ON DATABASE content_hub TO content_hub_migrate;
GRANT CONNECT ON DATABASE content_hub TO content_hub_app, content_hub_public;
SQL
psql_admin -d content_hub << SQL
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT USAGE, CREATE ON SCHEMA public TO content_hub_migrate;
GRANT USAGE ON SCHEMA public TO content_hub_app, content_hub_public;
ALTER DEFAULT PRIVILEGES FOR ROLE content_hub_migrate IN SCHEMA public GRANT SELECT ON TABLES TO content_hub_app, content_hub_public;
CREATE SCHEMA hub AUTHORIZATION content_hub_migrate;
GRANT USAGE ON SCHEMA hub TO content_hub_app;
ALTER DEFAULT PRIVILEGES FOR ROLE content_hub_migrate IN SCHEMA hub GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO content_hub_app;
SQL
db_url() { echo "postgres://$1:$2@127.0.0.1:${pgport}/content_hub?sslmode=verify-full&sslrootcert=/etc/postgres-ca/ca.crt"; }

common=(--network host --read-only --cap-drop ALL --security-opt no-new-privileges
  -v "${pgdir}/certs/ca.crt:/etc/postgres-ca/ca.crt:ro" -e DB_POOL_SIZE=2)
# Production settings with example values (no request below carries an Access token, so nothing is fetched).
public_env=(-e HOME_PAGE_URL=https://roadmap.example.org -e FEEDS_BASE_URL=https://feeds.example.org)
admin_env=(-e ACCESS_TEAM_DOMAIN=https://team.example.org -e ACCESS_AUD=smoke -e ADMIN_ORIGIN=https://hub.example.org
  -e KETO_READ_URL=http://127.0.0.1:1)

expect() { # expect <port> <method> <path> <status>
  local got
  got="$(curl -s -o /dev/null -w '%{http_code}' -X "$2" "http://127.0.0.1:$1$3")"
  if [ "$got" != "$4" ]; then
    echo "FAIL: $2 $3 on :$1 answered ${got}, want $4" >&2
    exit 1
  fi
}
wait_up() {
  for _ in $(seq 1 40); do
    curl -s -o /dev/null "http://127.0.0.1:$1/healthz" && return 0
    sleep 0.25
  done
  echo "FAIL: nothing listens on :$1" >&2
  exit 1
}

# 2. The binary.
"$docker" run --rm "${common[@]}" "$image" --version > /dev/null

# 3. Before migrating: up, not ready.
public="$("$docker" run -d "${common[@]}" "${public_env[@]}" -e "DATABASE_URL=$(db_url content_hub_public "$pub_pw")" \
  -e LISTEN_ADDR="127.0.0.1:${public_port}" "$image" public)"
admin="$("$docker" run -d "${common[@]}" "${admin_env[@]}" -e "DATABASE_URL=$(db_url content_hub_app "$app_pw")" \
  -e LISTEN_ADDR="127.0.0.1:${admin_port}" "$image" admin)"
worker="$("$docker" run -d "${common[@]}" -e "DATABASE_URL=$(db_url content_hub_app "$app_pw")" \
  -e LISTEN_ADDR="127.0.0.1:${worker_port}" "$image" worker)"
wait_up "$public_port"
wait_up "$admin_port"
wait_up "$worker_port"
for p in "$public_port" "$admin_port" "$worker_port"; do
  expect "$p" GET /healthz 200
  expect "$p" GET /readyz 503
done

# 4. An application role cannot migrate; the owner can, twice.
if "$docker" run --rm "${common[@]}" -e "DATABASE_URL=$(db_url content_hub_app "$app_pw")" "$image" migrate > /dev/null 2>&1; then
  echo "FAIL: the writer role ran the migrations" >&2
  exit 1
fi
for _ in 1 2; do
  "$docker" run --rm "${common[@]}" -e "DATABASE_URL=$(db_url content_hub_migrate "$own_pw")" "$image" migrate > /dev/null
done

# 5. Ready, and each process serves only its own routes.
expect "$public_port" GET /readyz 200
expect "$admin_port" GET /readyz 200
expect "$worker_port" GET /readyz 200
metrics="$(curl -s "http://127.0.0.1:${worker_port}/metrics")"
for want in 'content_hub_instances{status="queued"} 0' 'content_hub_worker_live 0' 'content_hub_publishing_paused 0'; do
  if ! grep -qF "$want" <<< "$metrics"; then
    echo "FAIL: /metrics lacks: $want" >&2
    exit 1
  fi
done
expect "$worker_port" GET /api/v1/me 404
expect "$public_port" GET /public/v1/roadmap 200
expect "$public_port" GET /public/v1/roadmap-bot 200
expect "$public_port" GET /public/v1/feed.json 200
expect "$public_port" GET /public/v1/feed.xml 200
expect "$public_port" GET /public/v1/openapi.json 200
expect "$public_port" POST /public/v1/roadmap 405
expect "$public_port" GET /api/v1/me 404
expect "$public_port" POST /api/v1/posts 404
expect "$admin_port" GET /api/v1/me 401
expect "$admin_port" GET /api/v1/phases 401
expect "$admin_port" GET /public/v1/roadmap 404

# 6. The generic platform definitions: the writer seeds them (idempotent); the read-only role cannot.
for _ in 1 2; do
  "$docker" run --rm "${common[@]}" -e "DATABASE_URL=$(db_url content_hub_app "$app_pw")" "$image" seed > /dev/null
done
if "$docker" run --rm "${common[@]}" -e "DATABASE_URL=$(db_url content_hub_public "$pub_pw")" "$image" seed > /dev/null 2>&1; then
  echo "FAIL: the read-only role wrote the platform definitions" >&2
  exit 1
fi

# 7. A misconfigured worker refuses to start.
if "$docker" run --rm "${common[@]}" -e "DATABASE_URL=$(db_url content_hub_app "$app_pw")" -e PUBLISHING_LIVE=yes \
  "$image" worker > /dev/null 2>&1; then
  echo "FAIL: the worker started with PUBLISHING_LIVE=yes" >&2
  exit 1
fi

# 8. The snapshot: a key is generated privately; an export signs and records; the public process serves it.
key_out="$("$docker" run --rm "${common[@]}" --tmpfs /tmp "$image" snapshot-key --out /tmp/seed)"
if ! grep -Eq '"address":"0x[0-9a-f]{64}"' <<< "$key_out"; then
  echo "FAIL: snapshot-key did not print an address: $key_out" >&2
  exit 1
fi
if grep -Eqi 'seed"?:' <<< "${key_out//seed_file/}"; then
  echo "FAIL: snapshot-key printed something seed-like: $key_out" >&2
  exit 1
fi
seed="$(openssl rand -base64 32)"
addr="$("$docker" run --rm "${common[@]}" -e "SNAPSHOT_SIGNER_SEED=$seed" "$image" snapshot-address)"
[[ "$addr" =~ ^0x[0-9a-f]{64}$ ]] || { echo "FAIL: snapshot-address printed '$addr'" >&2; exit 1; }
export_run() {
  "$docker" run --rm "${common[@]}" -e "DATABASE_URL=$(db_url content_hub_app "$app_pw")" -e "SNAPSHOT_SIGNER_SEED=$seed" "$image" export-snapshot 2> /dev/null
}
first="$(export_run)"
grep -q '"outcome":"signed"' <<< "$first" || { echo "FAIL: first export: $first" >&2; exit 1; }
second="$(export_run)"
grep -q '"outcome":"unchanged"' <<< "$second" || { echo "FAIL: second export: $second" >&2; exit 1; }
served="$(curl -s "http://127.0.0.1:${public_port}/public/v1/snapshot")"
grep -q "\"signer_address\":\"${addr}\"" <<< "$served" || { echo "FAIL: /public/v1/snapshot: $served" >&2; exit 1; }
expect "$public_port" GET /public/v1/snapshot?network=devnet 422
if "$docker" run --rm "${common[@]}" -e "DATABASE_URL=$(db_url content_hub_app "$app_pw")" -e "SNAPSHOT_SIGNER_SEED=not-a-seed" "$image" export-snapshot > /dev/null 2>&1; then
  echo "FAIL: export-snapshot accepted a bad seed" >&2
  exit 1
fi
if "$docker" run --rm "${common[@]}" -e "DATABASE_URL=$(db_url content_hub_app "$app_pw")" -e "SNAPSHOT_SIGNER_SEED=$seed" -e SNAPSHOT_NETWORK=mainnet \
  "$image" export-snapshot > /dev/null 2>&1; then
  echo "FAIL: a mainnet export ran without SNAPSHOT_ALLOW_MAINNET=1" >&2
  exit 1
fi

# 9. Erasing a person (docs/PERSONAL_DATA.md): the owner-run script removes every trace of a subject, keeps the audit log's
# shape and its append-only trigger, refuses an unknown subject, and the application role still cannot touch the log.
psql_admin -d content_hub << 'SQL'
INSERT INTO hub.users (subject, display_name) VALUES ('smoke-person', 'smoke.person@example.org'), ('smoke-other', 'Other Person');
INSERT INTO hub.posts (title, body_markdown, status, created_by) VALUES ('Erase me', 'x', 'draft', 'smoke-person'), ('Keep me', 'y', 'draft', 'smoke-other');
INSERT INTO hub.audit_log (actor_subject, action, entity_type, entity_id, after)
  VALUES ('smoke-person', 'post.create', 'post', '1', '{"created_by": "smoke-person", "title": "Erase me"}'), ('smoke-other', 'post.create', 'post', '2', '{"created_by": "smoke-other"}');
SQL
"$docker" cp "$here/erase-person.sql" "$pgname":/tmp/erase-person.sql
owner_psql() { "$docker" exec -i -e "PGPASSWORD=$own_pw" "$pgname" psql -X -q -U content_hub_migrate -h 127.0.0.1 -d content_hub "$@"; }
if owner_psql -v subject=smoke-nobody -f /tmp/erase-person.sql > /dev/null 2>&1; then
  echo "FAIL: erase-person.sql accepted a subject nothing is stored for" >&2; exit 1
fi
out="$(owner_psql -v subject=smoke-person -f /tmp/erase-person.sql 2>&1)" \
  || { echo "FAIL: erase-person.sql failed: $out" >&2; exit 1; }
left="$(psql_admin -d content_hub -At -c "SELECT (SELECT count(*) FROM hub.users WHERE subject = 'smoke-person') + (SELECT count(*) FROM hub.posts WHERE created_by = 'smoke-person') + (SELECT count(*) FROM hub.audit_log WHERE actor_subject = 'smoke-person' OR after::text LIKE '%smoke-person%')")"
[[ "$left" == 0 ]] || { echo "FAIL: $left trace(s) of the erased subject remain" >&2; exit 1; }
kept="$(psql_admin -d content_hub -At -c "SELECT (SELECT count(*) FROM hub.users WHERE subject = 'smoke-other') + (SELECT count(*) FROM hub.posts WHERE created_by = 'smoke-other') + (SELECT count(*) FROM hub.audit_log WHERE actor_subject = 'smoke-other') + (SELECT count(*) FROM hub.audit_log WHERE actor_subject LIKE 'erased-%' AND after::text LIKE '%erased-%')")"
[[ "$kept" == 4 ]] || { echo "FAIL: expected the other person's rows and the pseudonymised audit row to remain (got $kept)" >&2; exit 1; }
trg="$(psql_admin -d content_hub -At -c "SELECT tgenabled FROM pg_trigger WHERE tgname = 'audit_log_append_only'")"
[[ "$trg" == O ]] || { echo "FAIL: the audit log trigger is not enabled after the erasure" >&2; exit 1; }
if psql_admin -d content_hub -c "SET ROLE content_hub_app; UPDATE hub.audit_log SET action = 'x.y'" > /dev/null 2>&1; then
  echo "FAIL: the application role can update the audit log" >&2; exit 1
fi

echo "smoke: ok"
