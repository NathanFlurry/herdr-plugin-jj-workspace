# jj workspaces

A [Herdr](https://herdr.dev) plugin to create and destroy [Jujutsu](https://jj-vcs.github.io/jj/) (`jj`) workspaces with one keypress. Requires `jj` and `jq` on your `PATH`.

## Install

1. Install the plugin:

   ```sh
   herdr plugin install NathanFlurry/herdr-plugin-jj-workspace
   ```

   For local development, link a checkout instead: `herdr plugin link .`

2. Bind keys in your Herdr keybindings config (mirrors `new_worktree` =
   `prefix+shift+g`):

   ```toml
   [[keys.command]]
   key = "prefix+shift+j"
   type = "plugin_action"
   command = "nathanflurry.jj-workspace.new"
   description = "new jj workspace"

   [[keys.command]]
   key = "prefix+ctrl+j"
   type = "plugin_action"
   command = "nathanflurry.jj-workspace.remove"
   description = "remove jj workspace"
   ```

## Quickstart

- `prefix+shift+j` — create a workspace (prompts for a name)
- `prefix+ctrl+j` — destroy the current workspace

Replaces the manual `new tab → jj workspace add → cd` dance.

## License

MIT — see [LICENSE](LICENSE).
