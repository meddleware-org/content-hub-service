#!/usr/bin/env bash
# Fails unless an SBOM lists every crate linked into a binary at its locked version.
#
#   bash scripts/check-sbom.sh <sbom.json> <package> [<package> ...]
#
# <sbom.json> is CycloneDX (cargo-cyclonedx) or SPDX JSON; each <package> is a workspace package whose
# binary the SBOM describes (content-hub). The expected set is every crate
# reachable from those packages through normal (not dev, not build) dependency edges for the release
# targets, read from `cargo metadata --locked` (CARGO_METADATA=<file> to use a saved copy). An SBOM
# that misses one, lists it at another version, or lists nothing stops the release.
set -euo pipefail

sbom="${1:?usage: check-sbom.sh <sbom.json> <package> [<package> ...]}"
shift
[ "$#" -gt 0 ] || { echo "usage: check-sbom.sh <sbom.json> <package> [<package> ...]" >&2; exit 2; }
roots="$(printf '%s\n' "$@" | jq -R . | jq -sc .)"

if [ -n "${CARGO_METADATA:-}" ]; then
  metadata="$(cat "$CARGO_METADATA")"
else
  metadata="$(cargo metadata --locked --format-version 1 --filter-platform x86_64-unknown-linux-gnu)"
fi

# Crates reachable from the roots through normal edges (dep_kinds with kind null), as name@version.
expected="$(jq -r --argjson roots "$roots" '
  (.packages | map({key: .id, value: "\(.name)@\(.version)"}) | from_entries) as $names
  | (.resolve.nodes | map({key: .id, value: [.deps[] | select(any(.dep_kinds[]; .kind == null)) | .pkg]}) | from_entries) as $edges
  | [.packages[] | select(.name as $n | $roots | index($n)) | .id] as $start
  | if ($start | length) != ($roots | length) then error("unknown package in \($roots)") else . end
  | [$start | recurse(map($edges[.][]?) | unique; length > 0)] | flatten | unique
  | map($names[.]) | .[]
' <<< "$metadata" | sort -u)"

listed="$(jq -r '
  if .bomFormat == "CycloneDX" then
    [.metadata.component // empty, (.. | objects | select(has("components")) | .components[])]
    | map("\(.name)@\(.version)")
  elif has("spdxVersion") then
    [.packages[]? | "\(.name)@\(.versionInfo)"]
  else error("not a CycloneDX or SPDX JSON document") end
  | .[]
' "$sbom" | sort -u)"

if [ -z "$listed" ]; then
  echo "${sbom}: no packages" >&2
  exit 1
fi
total="$(grep -c . <<< "$expected" || true)"
if [ "$total" -eq 0 ]; then
  echo "${sbom}: the dependency graph lists no crates" >&2
  exit 1
fi
missing="$(comm -23 <(echo "$expected") <(echo "$listed"))"
if [ -n "$missing" ]; then
  echo "${sbom}: missing $(grep -c . <<< "$missing") of ${total} crates: $(tr '\n' ' ' <<< "$missing")" >&2
  exit 1
fi
echo "${sbom}: lists all ${total} crates of $*"
