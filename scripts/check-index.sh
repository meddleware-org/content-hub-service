#!/usr/bin/env bash
# Fails unless a multi-arch image index is what the release publishes: an OCI/Docker index holding one
# linux/amd64 and one linux/arm64 image, each with its BuildKit provenance attestation manifest.
#
#   bash scripts/check-index.sh <image-ref>        # e.g. quay.io/meddleware-org/content-hub-service@sha256:...
#   INDEX_JSON=<file> bash scripts/check-index.sh   # check a saved `imagetools inspect --raw` instead
#
# The release builds each architecture on its own native runner and merges the two per-architecture
# indexes with `docker buildx imagetools create`; this proves the merge kept both images and both
# attestations (the cosign signature and the build-provenance attestation are over the merged digest).
set -euo pipefail

if [ -n "${INDEX_JSON:-}" ]; then
  index="$(cat "$INDEX_JSON")"
else
  ref="${1:?usage: check-index.sh <image-ref>}"
  index="$(docker buildx imagetools inspect --raw "$ref")"
fi

jq -e '
  def images: [.manifests[] | select(.platform.os != "unknown" and .platform.architecture != "unknown")];
  def attestations: [.manifests[] | select(.annotations["vnd.docker.reference.type"] == "attestation-manifest")];
  (.mediaType == "application/vnd.oci.image.index.v1+json"
    or .mediaType == "application/vnd.docker.distribution.manifest.list.v2+json")
  and ([images[] | "\(.platform.os)/\(.platform.architecture)"] | sort) == ["linux/amd64", "linux/arm64"]
  and (attestations | length) == 2
  and ([attestations[].annotations["vnd.docker.reference.digest"]] | sort) == ([images[].digest] | sort)
' <<< "$index" > /dev/null || {
  echo "::error::the image index is not exactly {linux/amd64, linux/arm64} plus one provenance attestation each:" >&2
  jq -c '[.manifests[] | {digest, platform, type: .annotations["vnd.docker.reference.type"]}]' <<< "$index" >&2
  exit 1
}
echo "index ok: linux/amd64 + linux/arm64, each with its provenance attestation"
