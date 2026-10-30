# =============================================================================
# content-hub-service — one image, one binary: /app/content-hub <admin|public|worker|migrate|seed|export-snapshot>.
# =============================================================================
# Multi-stage build with cargo-chef for dependency-layer caching:
#   chef     — the Rust toolchain plus cargo-chef
#   planner  — computes the dependency recipe from Cargo.toml/lock
#   notices  — cargo-about licence notices of each binary (fails on an unaccepted licence)
#   builder  — compiles the dependencies (cached), then the binary (--locked; SQLx checks queries against the
#              committed offline metadata in .sqlx/, so no database is needed to build)
#   runtime  — distroless/cc: glibc + CA certificates; no shell, no package manager
#
# Storage is PostgreSQL over rustls (ring; the database CA is mounted at run time), so the runtime needs only
# glibc. Both base images are pinned by digest
# (multi-arch index); bump tag and digest together
# (`skopeo inspect --format '{{.Digest}}' docker://<image>:<tag>`).
# =============================================================================

FROM docker.io/library/rust:1-slim@sha256:4df2ae8cfdfebdfc13a65da7f253b73d68983dc0339f2719bc17a279c913b13d AS chef
RUN cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /src

# ── Planner: derive the dependency recipe ────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Licence notices (Apache-2.0 §4(a) and the MIT/BSD notice clauses) ────────
# `--fail` stops the build when a linked crate's licence is not accepted by about.toml. CI builds this
# target alone (`--target notices`) so a licence problem shows up without a full compile.
FROM chef AS notices
RUN cargo install --locked --version 0.9.2 --features cli cargo-about
COPY . .
RUN mkdir -p /out \
 && cargo about generate --locked --fail -c about.toml --manifest-path crates/cli/Cargo.toml about.hbs \
      > /out/THIRD_PARTY_LICENSES.txt \
 && grep -q 'Apache License' /out/THIRD_PARTY_LICENSES.txt

# ── Builder: compile deps (cached), then the binary ──────────────────────────
FROM chef AS builder
ENV SQLX_OFFLINE=true
COPY --from=planner /src/recipe.json recipe.json
# Re-used on every build where only the sources change (not Cargo.lock).
RUN cargo chef cook --release --locked --recipe-path recipe.json -p content-hub
COPY . .
RUN cargo build --release --locked -p content-hub \
 && install -D -m 0755 target/release/content-hub /out/content-hub

# ── Runtime: minimal distroless image ────────────────────────────────────────
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2

ARG VERSION=dev
ARG VENDOR="Meddleware"
ARG DESCRIPTION="Content hub: canonical posts, roadmap and publishing queue (admin, public, worker)."
ARG SOURCE_URL="https://github.com/meddleware-org/content-hub-service"
ARG DOCUMENTATION_URL="https://github.com/meddleware-org/content-hub-service#readme"
ARG IMAGE_URL="https://quay.io/meddleware-org/content-hub-service"

LABEL org.opencontainers.image.title="content-hub-service" \
      org.opencontainers.image.description="${DESCRIPTION}" \
      org.opencontainers.image.url="${IMAGE_URL}" \
      org.opencontainers.image.source="${SOURCE_URL}" \
      org.opencontainers.image.documentation="${DOCUMENTATION_URL}" \
      org.opencontainers.image.vendor="${VENDOR}" \
      org.opencontainers.image.licenses="0BSD" \
      org.opencontainers.image.version="${VERSION}"

WORKDIR /app
COPY --from=builder /out/content-hub /app/
COPY --from=notices /out/ /app/
# The lockfile makes an image scan (Trivy, syft) list the compiled crates (a stripped binary carries
# none); it is inert at runtime. The release also attests a Cargo SBOM of the binary.
COPY Cargo.lock /app/Cargo.lock
COPY LICENSE /app/LICENSE

# 65532 is distroless's nonroot user (the pod's runAsUser and the volume's fsGroup).
USER 65532:65532
EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080

# The subcommand is the process: every Deployment, Job and CronJob sets its `args:` (admin, public, worker,
# migrate, seed, export-snapshot). No volume: all state is in PostgreSQL (DATABASE_URL, with the CA at
# /etc/postgres-ca/ca.crt).
ENTRYPOINT ["/app/content-hub"]
CMD ["--help"]
