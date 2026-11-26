#!/usr/bin/env bash
# Offline tests of scripts/check-sbom.sh (the release gate keeping the attested Cargo SBOMs complete).
# A fixed `cargo metadata` graph: app → a (normal) → d (normal); app → b (dev only); app → c (build only)
# → e (normal under the build dependency); other → f (a workspace package outside the roots).
set -euo pipefail
# The gate under test sits one directory up.
here="$(cd "$(dirname "$0")" && pwd)"
check="${here}/../check-sbom.sh"
# A scratch directory for the fixture files, removed on exit.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# The fixed dependency graph described in the header, in the shape `cargo metadata` prints.
cat > "$tmp/metadata.json" <<'JSON'
{
  "packages": [
    {"id": "app", "name": "app", "version": "0.0.12"},
    {"id": "a", "name": "a", "version": "1.0.0"},
    {"id": "b", "name": "b", "version": "2.0.0"},
    {"id": "c", "name": "c", "version": "3.0.0"},
    {"id": "d", "name": "d", "version": "4.0.0"},
    {"id": "e", "name": "e", "version": "5.0.0"},
    {"id": "other", "name": "other", "version": "0.0.12"},
    {"id": "f", "name": "f", "version": "6.0.0"}
  ],
  "resolve": {"nodes": [
    {"id": "app", "deps": [
      {"pkg": "a", "dep_kinds": [{"kind": null}]},
      {"pkg": "b", "dep_kinds": [{"kind": "dev"}]},
      {"pkg": "c", "dep_kinds": [{"kind": "build"}]}
    ]},
    {"id": "a", "deps": [{"pkg": "d", "dep_kinds": [{"kind": null}, {"kind": "dev"}]}]},
    {"id": "b", "deps": []},
    {"id": "c", "deps": [{"pkg": "e", "dep_kinds": [{"kind": null}]}]},
    {"id": "d", "deps": []},
    {"id": "e", "deps": []},
    {"id": "other", "deps": [{"pkg": "f", "dep_kinds": [{"kind": null}]}]},
    {"id": "f", "deps": []}
  ]}
}
JSON
# Tells the gate to read this file instead of running cargo.
export CARGO_METADATA="$tmp/metadata.json"

# Build a CycloneDX SBOM listing the given name@version crates, with `app` as its main component.
cdx() { # cdx <name@version>... → a CycloneDX document with app as the metadata component
  local comps
  comps="$(printf '%s\n' "$@" | jq -R 'split("@") | {type: "library", name: .[0], version: .[1]}' | jq -sc .)"
  jq -n --argjson c "$comps" '{bomFormat: "CycloneDX", specVersion: "1.5", metadata: {component: {name: "app", version: "0.0.12"}}, components: $c}'
}

# Counts the cases that behaved as expected.
pass=0
# Run the gate on one SBOM and fail the whole test unless it exits zero (want 0) or non-zero (want 1) as stated.
expect() { # expect <0|1 (non-zero)> <description> <sbom-json> [packages...]
  local want="$1" what="$2" doc="$3"
  shift 3
  echo "$doc" > "$tmp/sbom.json"
  set +e
  bash "$check" "$tmp/sbom.json" "$@" > "$tmp/out" 2>&1
  local got=$?
  set -e
  if { [ "$want" = 0 ] && [ "$got" != 0 ]; } || { [ "$want" != 0 ] && [ "$got" = 0 ]; }; then
    echo "FAIL: ${what}: exit ${got}, want $([ "$want" = 0 ] && echo 0 || echo 'non-zero')" >&2
    cat "$tmp/out" >&2
    exit 1
  fi
  pass=$((pass + 1))
}

# A complete SBOM passes, and the success message gives the count.
expect 0 "lists every linked crate (dev and build-only crates are not expected)" "$(cdx a@1.0.0 d@4.0.0)" app
grep -q 'lists all 3 crates' "$tmp/out" || { echo "FAIL: count" >&2; cat "$tmp/out" >&2; exit 1; }
# A missing crate or a different version fails, and the message names the missing crate.
expect 1 "names a crate the SBOM omits" "$(cdx a@1.0.0)" app
grep -q 'd@4.0.0' "$tmp/out" || { echo "FAIL: the missing crate is not named" >&2; exit 1; }
expect 1 "a version mismatch is a miss" "$(cdx a@1.0.1 d@4.0.0)" app
expect 1 "an SBOM without packages" '{"bomFormat": "CycloneDX", "components": []}' app
# Both CycloneDX nesting and SPDX are read.
expect 0 "nested CycloneDX components count" \
  '{"bomFormat":"CycloneDX","metadata":{"component":{"name":"app","version":"0.0.12","components":[{"name":"a","version":"1.0.0"}]}},"components":[{"name":"d","version":"4.0.0"}]}' app
expect 0 "SPDX documents are read too" \
  '{"spdxVersion":"SPDX-2.3","packages":[{"name":"app","versionInfo":"0.0.12"},{"name":"a","versionInfo":"1.0.0"},{"name":"d","versionInfo":"4.0.0"}]}' app
# More than one root package uses the union of their graphs; an unknown root or a non-SBOM fails.
expect 0 "several roots: the union of their graphs" "$(cdx a@1.0.0 d@4.0.0 other@0.0.12 f@6.0.0)" app other
expect 1 "several roots: a crate of the second root missing" "$(cdx a@1.0.0 d@4.0.0 other@0.0.12)" app other
expect 1 "an unknown root package" "$(cdx a@1.0.0 d@4.0.0)" nope
expect 1 "not an SBOM" '{"hello": 1}' app

# Without CARGO_METADATA the graph is read with `cargo metadata --locked` for the release platform: a stub cargo records its arguments.
# A stub `cargo` that records its arguments and prints the fixture, to check how the gate calls it.
mkdir -p "$tmp/bin"
cat > "$tmp/bin/cargo" <<'STUB'
#!/usr/bin/env bash
echo "$*" > "$STUB_ARGS"
cat "$STUB_METADATA"
STUB
chmod +x "$tmp/bin/cargo"
unset CARGO_METADATA
export STUB_METADATA="$tmp/metadata.json" STUB_ARGS="$tmp/args"
echo "$(cdx a@1.0.0 d@4.0.0)" > "$tmp/sbom.json"
PATH="$tmp/bin:$PATH" bash "$check" "$tmp/sbom.json" app > /dev/null
grep -q -- '--locked' "$tmp/args" && grep -q -- '--filter-platform x86_64-unknown-linux-gnu' "$tmp/args" || { echo "FAIL: cargo metadata must be locked and filtered to the release platform: $(cat "$tmp/args")" >&2; exit 1; }
pass=$((pass + 1))
echo "check-sbom: ${pass} cases passed"
