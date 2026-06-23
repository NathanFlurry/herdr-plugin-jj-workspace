#!/usr/bin/env bash
# Headless action: resolve the focused workspace's repo from context, then open
# the interactive create pane and hand it the repo via --env (like the
# github-link-preview example passes GITHUB_URL).
set -euo pipefail

herdr_bin="${HERDR_BIN_PATH:-herdr}"
ctx="${HERDR_PLUGIN_CONTEXT_JSON:-}"
[ -n "$ctx" ] || ctx='{}'

repo=""
if command -v jq >/dev/null 2>&1; then
  repo=$(jq -r '.workspace_cwd // .focused_pane_cwd // empty' <<<"$ctx")
fi

exec "$herdr_bin" plugin pane open \
  --plugin "${HERDR_PLUGIN_ID:-nathanflurry.jj-workspace}" \
  --entrypoint new \
  --env "JJ_REPO=$repo" \
  --focus
