# spindle

A keyboard-first, agent-aware terminal multiplexer: real PTY panes, a sidebar of workspaces and running agents, a quiet sound when an agent needs attention, and a built-in git changeset viewer.

Status: early development. Today spindle runs your `$SHELL` (falling back to `/bin/sh`) in panes you can split, grouped into workspaces, with a review view for git changes (below). The left sidebar has the menu on top, then your workspaces, then agents; the agents list is still a static placeholder, not detected agents.

Every pane and workspace starts in the directory spindle was launched from. When a pane's shell exits the pane closes and its neighbours take its space; a workspace with no panes left goes away, and spindle exits when the last shell does.

## Keys

The prefix is `Ctrl-b`, then:

| Key | Action |
| --- | --- |
| `n` | New workspace |
| `s` / `S` | Split the focused pane side by side / stacked |
| `x` | Close the focused pane (kills its shell) |
| `o` | Focus the next pane |
| arrows | Focus the pane in that direction |
| `1`–`9` | Switch to that workspace |
| `g` | Open or close the review view |
| `q` | Quit spindle (kills every shell) |
| `Ctrl-b` | Send a literal `Ctrl-b` to the shell |

Any other key after the prefix is passed to the shell along with the `Ctrl-b`. Splitting along the same direction again adds an equal-width sibling rather than halving the pane, as in tmux, and a pane too small to halve won't split. With more than one pane, the focused one has a cyan border.

## Mouse

Click any command in the sidebar menu to run it, a workspace in the sidebar to switch to it, or a pane to focus it. In the review view, click a file to show it or a folder to fold it, and use the wheel to scroll the diff.

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
