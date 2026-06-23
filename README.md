# jj workspaces

A [Herdr](https://herdr.dev) plugin to create and destroy [Jujutsu](https://jj-vcs.github.io/jj/) (`jj`) workspaces with one keypress. Requires `jj` and `jq` on your `PATH`.

## Install

```sh
herdr plugin install NathanFlurry/herdr-plugin-jj-workspace
```

For local development, link a checkout instead: `herdr plugin link .`

## Keybindings

Actions aren't bound by default. Add to your Herdr keybindings config:

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

## Quickstart

### Create workspace

Press `prefix+j`, type a name. Runs `jj workspace add` and opens the new
workspace as a focused Herdr workspace.

### Destroy workspace

Press `prefix+J` inside a jj workspace. Runs `jj workspace forget`, deletes the
directory, and closes the Herdr workspace. The main workspace is never removed.

## License

MIT — see [LICENSE](LICENSE).
