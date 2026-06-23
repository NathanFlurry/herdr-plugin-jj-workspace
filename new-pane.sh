#!/usr/bin/env bash
# Runs inside a Herdr overlay pane (has a TTY -> can prompt).
#
# Flow (triggered by `prefix+j`): prompt for a name, run `jj workspace add`, then
# open the new workspace as a focused Herdr workspace. New workspaces land under
# $JJ_WORKSPACE_ROOT if set, otherwise next to the repo as "<repo>.<name>".
# The repo path arrives as $JJ_REPO from open-new.sh.
set -euo pipefail

herdr_bin="${HERDR_BIN_PATH:-herdr}"

finish() { printf '\npress enter to close...'; read -r _ || true; }
trap finish EXIT

[ -f "${HERDR_PLUGIN_CONFIG_DIR:-}/.env" ] && source "$HERDR_PLUGIN_CONFIG_DIR/.env"

command -v jj >/dev/null 2>&1 || { echo "error: jj not found on PATH"; exit 0; }

repo="${JJ_REPO:-}"
if [ -z "$repo" ] || [ ! -e "$repo/.jj" ]; then
  read -e -rp "jj repo path: " repo
fi
repo="${repo%/}"
[ -e "$repo/.jj" ] || { echo "error: $repo is not a jj workspace"; exit 0; }

read -e -rp "new workspace name: " name
[ -n "$name" ] || { echo "no name given"; exit 0; }

# Destination: $JJ_WORKSPACE_ROOT/<repo>.<name> if configured, else a sibling dir.
base="$(basename "$repo")"
if [ -n "${JJ_WORKSPACE_ROOT:-}" ]; then
  dest="${JJ_WORKSPACE_ROOT%/}/${base}.${name}"
else
  dest="$(dirname "$repo")/${base}.${name}"
fi

echo "+ jj workspace add --name $name $dest"
( cd "$repo" && jj workspace add --name "$name" "$dest" )

echo "+ herdr workspace create --cwd $dest"
"$herdr_bin" workspace create --cwd "$dest" --label "$name" --focus
echo "done."
