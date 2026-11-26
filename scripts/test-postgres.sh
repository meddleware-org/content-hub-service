#!/usr/bin/env bash
# A throwaway PostgreSQL 18 with TLS for the tests, the contract job and the smoke test (CI and local).
#
#   bash scripts/test-postgres.sh start    # starts the container, writes $PG_DIR/env.sh (and $GITHUB_ENV)
#   . "$PG_DIR/env.sh"                     # TEST_DATABASE_URL (superuser, sslmode=verify-full), REQUIRE_TEST_DATABASE=1
#   bash scripts/test-postgres.sh stop
#
# DOCKER=podman works rootless. The server certificate (SAN localhost, 127.0.0.1) is signed by a throwaway CA
# generated here, so the tests exercise the same `verify-full` path as production. The image is pinned by
# digest (the same postgres:18 the platform's database tier runs); the password is random per start and only
# ever in the container's environment and $PG_DIR/env.sh.
set -euo pipefail

# Settings, each overridable from the environment: the container tool, container name, host port, image and state directory.
docker="${DOCKER:-docker}"
name="${PG_NAME:-content-hub-test-pg}"
port="${PG_PORT:-55433}"
image="${PG_IMAGE:-docker.io/library/postgres:18@sha256:06cad38a5d9f5d24b4d83d86def30795d5e4b757fedbf5281172b576dedcd941}"
dir="${PG_DIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/content-hub-test-pg}"

# One argument selects the action; anything else prints the usage and exits 2.
case "${1:-}" in
  start)
    # A throwaway CA and a server certificate valid for three days, regenerated on every start.
    mkdir -p "$dir/certs"
    (
      cd "$dir/certs"
      openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt -days 3 -subj "/CN=content-hub-test-ca" 2> /dev/null
      openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=localhost" 2> /dev/null
      printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\n' > ext.cnf
      openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out server.crt -days 3 -extfile ext.cnf 2> /dev/null
      chmod 0644 ca.crt server.crt server.key
    )
    # A random superuser password, written only to the container's environment and the env file below.
    password="$(openssl rand -hex 16)"
    # Remove a container left by an earlier run, if any.
    "$docker" rm -f "$name" > /dev/null 2>&1 || true
    # fsync off: the data is throwaway and the tests create hundreds of databases.
    "$docker" run -d --name "$name" -p "127.0.0.1:${port}:5432" -e POSTGRES_PASSWORD="$password" \
      -v "$dir/certs:/certs:ro" --entrypoint sh "$image" -c '
        install -o postgres -g postgres -m 600 /certs/server.key /var/lib/postgresql/server.key &&
        install -o postgres -g postgres -m 644 /certs/server.crt /var/lib/postgresql/server.crt &&
        exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/var/lib/postgresql/server.crt \
          -c ssl_key_file=/var/lib/postgresql/server.key -c max_connections=400 \
          -c fsync=off -c synchronous_commit=off -c full_page_writes=off' > /dev/null
    # Wait up to a minute for the server to accept connections.
    for _ in $(seq 1 60); do
      "$docker" exec "$name" pg_isready -q -h 127.0.0.1 -U postgres && break
      sleep 1
    done
    "$docker" exec "$name" pg_isready -h 127.0.0.1 -U postgres
    {
      # The env file for local use: source it to get the URL and the flag that stops database tests being skipped.
      echo "export TEST_DATABASE_URL='postgres://postgres:${password}@127.0.0.1:${port}/postgres?sslmode=verify-full&sslrootcert=${dir}/certs/ca.crt'"
      echo "export REQUIRE_TEST_DATABASE=1"
    } > "$dir/env.sh"
    # On GitHub Actions, the same values are exported to the later steps of the job.
    if [ -n "${GITHUB_ENV:-}" ]; then
      {
        echo "TEST_DATABASE_URL=postgres://postgres:${password}@127.0.0.1:${port}/postgres?sslmode=verify-full&sslrootcert=${dir}/certs/ca.crt"
        echo "REQUIRE_TEST_DATABASE=1"
      } >> "$GITHUB_ENV"
    fi
    echo "PostgreSQL ready on 127.0.0.1:${port} (env: ${dir}/env.sh, CA: ${dir}/certs/ca.crt)"
    ;;
  # Remove the container and its state directory; quiet if they are already gone.
  stop)
    "$docker" rm -f "$name" > /dev/null 2>&1 || true
    rm -rf "$dir"
    ;;
  *)
    echo "usage: test-postgres.sh start|stop" >&2
    exit 2
    ;;
esac
