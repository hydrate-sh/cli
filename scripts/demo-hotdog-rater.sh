#!/usr/bin/env bash
#
# End-to-end demo: author and commit a small graph with the hydrate CLI, with
# zero hand-written UUIDs or handles. Reproduces the "Hot Dog Rater" example.
#
# Requirements:
#   - a `hydrate` binary on PATH (or set HYDRATE=/path/to/hydrate)
#   - HYD_API_KEY and HYD_BASE_URL exported (the CLI reads them from the env)
#   - exactly one active project on the account (the common-path project rule)
#
# Usage:
#   HYD_API_KEY=… HYD_BASE_URL=… ./scripts/demo-hotdog-rater.sh
#
# It runs in a fresh temporary directory so it never disturbs your cwd.

set -euo pipefail

hydrate="${HYDRATE:-hydrate}"
branch="hotdog-rater-$(date +%s 2>/dev/null || echo demo)"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
cd "$workdir"

echo "==> fork a working branch ($branch) and bind this directory to it"
"$hydrate" fork "$branch"

echo "==> add a boundary and two behaviors with typed ports"
"$hydrate" node add --kind boundary --name Api --user-kind service
"$hydrate" node add --kind behavior --name Maker --parent Api --out dog:HotDog
"$hydrate" node add --kind behavior --name Rater --parent Api \
  --in raw:HotDog --out score:Score

echo "==> connect Maker's output to Rater's input (typed edge, by port path)"
"$hydrate" edge add --from Api.Maker.dog --to Api.Rater.raw

echo "==> inspect the staged changeset"
"$hydrate" status
"$hydrate" diff

echo "==> commit the batch to the branch"
"$hydrate" commit

echo "==> done. The graph is live on branch '$branch'."
