#!/usr/bin/env bash
# Offline tests of scripts/check-index.sh (the release gate proving the merged multi-arch index kept both
# architectures and their provenance attestations). Indexes are fed through INDEX_JSON.
set -euo pipefail
# The gate under test sits one directory up.
here="$(cd "$(dirname "$0")" && pwd)"
check="${here}/../check-index.sh"
# A scratch directory for the index files, removed on exit.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# The media type of an OCI image index, the form the release produces.
OCI="application/vnd.oci.image.index.v1+json"
# A manifest entry for an image of the given digest and architecture.
img() { # img <digest> <arch>
  jq -n --arg d "$1" --arg a "$2" '{digest: $d, platform: {os: "linux", architecture: $a}}'
}
# A manifest entry for a provenance attestation that refers to the image with the second digest.
att() { # att <digest> <of-digest>
  jq -n --arg d "$1" --arg o "$2" '{digest: $d, platform: {os: "unknown", architecture: "unknown"},
    annotations: {"vnd.docker.reference.type": "attestation-manifest", "vnd.docker.reference.digest": $o}}'
}
# An index document of the given media type listing the given entries.
index() { # index <mediaType> <manifest>...
  local mt="$1"
  shift
  printf '%s\n' "$@" | jq -s --arg mt "$mt" '{schemaVersion: 2, mediaType: $mt, manifests: .}'
}

# Counts the cases that behaved as expected.
pass=0
# Run the gate on one index and fail the whole test unless it exits zero (want 0) or non-zero (want 1) as stated.
expect() { # expect <0|1> <description> <index-json>
  local want="$1" what="$2"
  echo "$3" > "$tmp/index.json"
  set +e
  INDEX_JSON="$tmp/index.json" bash "$check" > "$tmp/out" 2>&1
  local got=$?
  set -e
  if { [ "$want" = 0 ] && [ "$got" != 0 ]; } || { [ "$want" != 0 ] && [ "$got" = 0 ]; }; then
    echo "FAIL: ${what}: exit ${got}, want $([ "$want" = 0 ] && echo 0 || echo 'non-zero')" >&2
    cat "$tmp/out" >&2
    exit 1
  fi
  pass=$((pass + 1))
}

# Two images and their attestations, the shape of a good release index.
A="$(img sha256:aa amd64)"; B="$(img sha256:bb arm64)"
AA="$(att sha256:a1 sha256:aa)"; BA="$(att sha256:b1 sha256:bb)"

# Accepted: a complete index in either order, or as a Docker manifest list.
expect 0 "both architectures, each with its attestation" "$(index "$OCI" "$A" "$AA" "$B" "$BA")"
expect 0 "the order of the manifests does not matter" "$(index "$OCI" "$A" "$B" "$AA" "$BA")"
expect 0 "a Docker manifest list is accepted" "$(index application/vnd.docker.distribution.manifest.list.v2+json "$A" "$AA" "$B" "$BA")"
# Refused: a missing architecture or attestation, an attestation pointing nowhere, an extra or duplicate architecture, or a plain image.
expect 1 "one architecture only (a per-architecture index)" "$(index "$OCI" "$A" "$AA")"
expect 1 "an architecture is missing its attestation" "$(index "$OCI" "$A" "$AA" "$B")"
expect 1 "an attestation points at no image of the index" "$(index "$OCI" "$A" "$AA" "$B" "$(att sha256:b1 sha256:zz)")"
expect 1 "a third architecture" "$(index "$OCI" "$A" "$AA" "$B" "$BA" "$(img sha256:cc s390x)")"
expect 1 "the same architecture twice" "$(index "$OCI" "$A" "$AA" "$B" "$BA" "$(img sha256:dd amd64)")"
expect 1 "a single image manifest, not an index" '{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[]}'
echo "check-index: ${pass} cases passed"
