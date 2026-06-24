# jj workspaces

A [Herdr](https://herdr.dev) plugin to create and remove [Jujutsu](https://jj-vcs.github.io/jj/) (`jj`) workspaces with one keypress — as a new Herdr workspace or a new tab. A small Rust binary; requires `jj` at runtime and `cargo` to build.

## Install

1. Install the plugin (Herdr builds it with `cargo` at install time):

   ```sh
   herdr plugin install NathanFlurry/herdr-plugin-jj-workspace
   ```

   For local development, `plugin link` does **not** build — build first:

   ```sh
   cargo build --release
   herdr plugin link .
   ```

2. Bind keys in your Herdr keybindings config (`prefix` is your leader, default
   `ctrl+b`). These are unbound in stock Herdr:

   ```toml
   [[keys.command]]
   key = "prefix+a"
   type = "plugin_action"
   command = "nathanflurry.jj-workspace.new"
   description = "new jj workspace"

   [[keys.command]]
   key = "prefix+shift+a"
   type = "plugin_action"
   command = "nathanflurry.jj-workspace.new-tab"
   description = "new jj workspace (in tab)"

   [[keys.command]]
   key = "prefix+d"
   type = "plugin_action"
   command = "nathanflurry.jj-workspace.remove"
   description = "remove jj workspace"
   ```

## Quickstart

- `prefix+a` — create a workspace (prompts for a name and base bookmark), open as a new **workspace**
- `prefix+shift+a` — same, but open in a new **tab**
- `prefix+d` — destroy the current workspace

New workspaces are based on the selected jj bookmark and do not create a new bookmark.
Replaces the manual `new tab → jj workspace add -r <bookmark> → cd` dance.

## License

MIT — see [LICENSE](LICENSE).
