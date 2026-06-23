# jj workspaces

A [Herdr](https://herdr.dev) plugin to create and destroy [Jujutsu](https://jj-vcs.github.io/jj/) (`jj`) workspaces with one keypress. Requires `jj` and `jq` on your `PATH`.

## Install

```sh
herdr plugin install NathanFlurry/herdr-plugin-jj-workspace
```

For local development, link a checkout instead: `herdr plugin link .`

## Quickstart

Spinning up a new workspace — before:

```sh
# open a new tab, then:
jj workspace add ../myrepo.feature
cd ../myrepo.feature
```

after:

```
prefix+j  →  type "feature"
```

Tearing it back down — before:

```sh
jj workspace forget feature
rm -rf ../myrepo.feature
# close the tab
```

after:

```
prefix+J
```

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

## License

MIT — see [LICENSE](LICENSE).
