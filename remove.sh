#!/usr/bin/env bash
# Headless action (triggered by `prefix+J` inside a jj workspace): run
# `jj workspace forget`, delete the directory, and close the Herdr workspace.
# The main workspace is never removed. Output goes to `herdr plugin log list`.
set -euo pipefail

herdr_bin="${HERDR_BIN_PATH:-herdr}"
ctx="${HERDR_PLUGIN_CONTEXT_JSON:-{}}"
ws="${HERDR_WORKSPACE_ID:-}"
cwd=""
if command -v jq >/dev/null 2>&1; then
  [ -z "$ws" ] && ws=$(jq -r '.workspace_id  // empty' <<<"$ctx")
  cwd=$(jq -r '.workspace_cwd // empty' <<<"$ctx")
fi
[ -n "$cwd" ] || { echo "no workspace cwd in context" >&2; exit 1; }

# Safety: never remove the MAIN workspace. The main workspace stores .jj/repo as
# a directory; a secondary workspace stores it as a file pointer to the store.
if [ -d "$cwd/.jj/repo" ]; then
  echo "refusing to remove the MAIN jj workspace ($cwd)" >&2; exit 1
fi
[ -e "$cwd/.jj" ] || { echo "$cwd is not a jj workspace" >&2; exit 1; }

( cd "$cwd" && jj workspace forget )   # forgets the current workspace
rm -rf "$cwd"
[ -n "$ws" ] && "$herdr_bin" workspace close "$ws"
echo "removed jj workspace: $cwd"
