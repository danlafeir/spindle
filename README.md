# spindle

A keyboard-first, agent-aware terminal multiplexer: real PTY panes, a sidebar of workspaces and running agents, a quiet sound when an agent needs attention, and a built-in git changeset viewer.

Status: early development. Today spindle shows a layout mock: a left sidebar with the menu on top, then workspaces and agents, and your `$SHELL` (falling back to `/bin/sh`) running in a pane on the right. The sidebar contents are static placeholders, not real workspaces or detected agents. The session ends when the shell exits.

## Keys

The prefix is `Ctrl-b`, then:

| Key | Action |
| --- | --- |
| `q` | Quit spindle (kills the shell) |
| `n`, `s` | Placeholders for new workspace and split; they do nothing yet |
| `Ctrl-b` | Send a literal `Ctrl-b` to the shell |

Any other key after the prefix is passed to the shell along with the `Ctrl-b`.

## Try it

```sh
cargo run
```

Run it from an interactive terminal; it exits with an error if stdin is not a TTY.
