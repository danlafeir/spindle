# spindle

A keyboard-first, agent-aware terminal multiplexer: real PTY panes, a sidebar of workspaces and running agents, a quiet sound when an agent needs attention, and a built-in git changeset viewer.

Status: early development. Today spindle runs your `$SHELL` (falling back to `/bin/sh`) in a single full-screen PTY pane, forwarding input, output and window resizes. The session ends when the shell exits. There are no panes, sidebar or keybindings yet.

## Try it

```sh
cargo run
```

Run it from an interactive terminal; it exits with an error if stdin is not a TTY.
