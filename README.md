# spindle

A keyboard-first, agent-aware terminal multiplexer: real PTY panes, a sidebar of workspaces and running agents, a quiet sound when an agent needs attention, and a built-in git changeset viewer.

Status: early development. Today spindle has a working review view for git changes (below), inside a layout mock: a left sidebar with the menu on top, then workspaces and agents, and your `$SHELL` (falling back to `/bin/sh`) running in a pane on the right. The sidebar contents are static placeholders, not real workspaces or detected agents. The session ends when the shell exits.

## Keys

The prefix is `Ctrl-b`, then:

| Key | Action |
| --- | --- |
| `g` | Open or close the review view |
| `q` | Quit spindle (kills the shell) |
| `n`, `s` | Placeholders for new workspace and split; they do nothing yet |
| `Ctrl-b` | Send a literal `Ctrl-b` to the shell |

Any other key after the prefix is passed to the shell along with the `Ctrl-b`.

## Mouse

Click `review` or `quit` in the sidebar menu to do the same as `Ctrl-b g` or `Ctrl-b q`. In the review view, click a file to show it or a folder to fold it, and use the wheel to scroll the diff.

spindle captures the mouse, so dragging no longer selects text. In Terminal.app, hold `Fn` while dragging to select, or turn off View → Allow Mouse Reporting.

## Reviewing changes

`Ctrl-b g` swaps the shell pane for a review of uncommitted changes: everything that differs from `HEAD`, staged and unstaged together. On the left is a tree of the whole repo (gitignored paths hidden) with changed files marked `M` modified, `A` added, `D` deleted, `R` renamed or `?` untracked. Folders containing changes start expanded. On the right is the selected file's diff, or its contents if it hasn't changed. The shell keeps running underneath.

| Key | Action |
| --- | --- |
| `j`/`k` or arrows | Move through the tree |
| `l`/`h` or arrows | Expand a folder / collapse it or jump to its parent |
| `Enter` or `o` | Toggle a folder |
| `g`/`G` | First / last entry |
| `J`/`K`, `Space`, PgDn/PgUp | Scroll the diff or file |
| `r` | Reload from git |
| `q` or `Esc` | Back to the shell |

The review shows the repo spindle was launched in, not wherever the shell has since `cd`'d to.

## Try it

```sh
cargo run
```

Run it from an interactive terminal; it exits with an error if stdin is not a TTY.
