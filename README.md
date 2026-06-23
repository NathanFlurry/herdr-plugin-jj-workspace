# jj workspaces

A [Herdr](https://herdr.dev) plugin that turns the
`new tab → jj workspace add → cd` dance into a single keypress, and tears a
workspace back down just as fast.

It shells out to [Jujutsu](https://jj-vcs.github.io/jj/) and calls back into
Herdr through `$HERDR_BIN_PATH` — there is no Herdr core change involved.

## What it does

- **New jj workspace** — prompts for a name, runs `jj workspace add`, then opens
  the new workspace as a focused Herdr workspace.
- **Remove jj workspace** — runs `jj workspace forget`, deletes the directory,
  and closes the Herdr workspace. Refuses to remove the main workspace.

## Requirements

- `jj` and `jq` on your `PATH`.

## Install

While developing locally:

```sh
herdr plugin link .
```

From GitHub:

```sh
herdr plugin install NathanFlurry/herdr-plugin-jj-workspace
```

## Keybindings

Add to your Herdr keybindings config:

```toml
[[keys.command]]
key = "prefix+j"
type = "plugin_action"
command = "nathanflurry.jj-workspace.new"
description = "new jj workspace"

[[keys.command]]
key = "prefix+J"
type = "plugin_action"
command = "nathanflurry.jj-workspace.remove"
description = "remove jj workspace"
```

Or invoke directly:

```sh
herdr plugin action invoke nathanflurry.jj-workspace.new
herdr plugin log list --plugin nathanflurry.jj-workspace   # headless output/errors
```

## Configuration

Optional. Set where new workspaces are created (defaults to a sibling directory
of the repo):

```sh
cp .env.example "$(herdr plugin config-dir nathanflurry.jj-workspace)/.env"
# then edit JJ_WORKSPACE_ROOT
```

## Known limitation

On stock Herdr, jj workspaces created by this plugin appear as **standalone**
workspaces — they are not nested under the parent repo, and the working-copy
bookmark / change id is not shown like a Git branch. That grouping and label
display is owned by Herdr core, not reachable from a plugin.
