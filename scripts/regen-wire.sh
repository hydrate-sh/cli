#!/usr/bin/env bash
# Regenerate the committed wire client (wire/) from the vendored openapi.json.
#
# The generated crate is the contract — never hand-edited. Run this whenever the
# vendored openapi.json changes; CI runs it and asserts no diff (the committed
# client must match the spec). Requires Java + npx (openapi-generator); it is NOT
# part of the cargo build — the CLI build only compiles the committed output.
# The generator version is pinned in openapitools.json (committed) for
# reproducible output.
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ ! -f openapi.json ]]; then
  echo "regen-wire: openapi.json not found at repo root" >&2
  exit 1
fi

# Clean-slate the generated tree first so a removed spec type can't leave an
# orphaned stale file behind (which a regenerate-and-diff guard would silently
# accept). We rewrite the ignore file right after, so from-scratch is deterministic.
rm -rf wire/src wire/Cargo.toml wire/.openapi-generator wire/.openapi-generator-ignore
mkdir -p wire
cat > wire/.openapi-generator-ignore <<'IGN'
# Keep the committed wire crate to the Rust client only — regen must be deterministic.
docs/
.travis.yml
git_push.sh
.gitignore
README.md
IGN

# Pin BOTH the wrapper (@2.38.0) and the generator jar (openapitools.json) for
# fully reproducible regen.
npx --yes @openapitools/openapi-generator-cli@2.38.0 generate \
  -i openapi.json \
  -g rust \
  --additional-properties=packageName=hydrate_wire,library=reqwest,supportMiddleware=false,preferUnsignedInt=true,useSingleRequestParameter=true \
  -o wire

# Deterministic post-step (part of regen, not a hand-edit): the generated client
# is not held to our lint bar — blanket-allow lints at the crate root so
# `clippy -D warnings` on the workspace stays green without us touching the
# generated code by hand. Idempotent: only injects if the marker is absent.
LIB=wire/src/lib.rs
MARKER="// hydrate-wire: generated crate — lints intentionally relaxed"
if ! grep -qF "$MARKER" "$LIB"; then
  TMP="$(mktemp)"
  {
    echo "$MARKER"
    echo "#![allow(clippy::all, clippy::pedantic, clippy::nursery)]"
    echo "#![allow(non_snake_case, dead_code, unused_imports, unused_qualifications)]"
    cat "$LIB"
  } > "$TMP"
  mv "$TMP" "$LIB"
fi

echo "regen-wire: wrote wire/ from openapi.json"
