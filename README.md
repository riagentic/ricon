# ricon

A fast terminal console with **vertical tabs** — built with [ratatui](https://ratatui.rs) + [portable-pty](https://crates.io/crates/portable-pty). Each tab holds one or more real shells; the sidebar shows folder, path, running process, git branch and live activity. ricon lives inside your existing terminal and aims to be the best console with vertical tabs.

```
┌ ricon ──┐
│▶1 ricon │   $ cargo build --release
│   ~/code/gen/ricon
│   └ cargo ⠹
│  ▶~/code/gen/ricon/src
│   └ vim
│         │
│ 2 ⭐web *│
│   ~/code/web
│   └ vite │
└──────────┘ 1/2 ▸ ~/code/gen/ricon  ⎇ main  ✳ claude-opus-4-8   v0.3.0
```

## Features

- 🗂️ **Vertical tabs** — entries show folder, full path, running process + activity spinner.
- 🐚 **Subshells** — `Alt+s` adds extra shells to a tab; they move with it, share its color, and add their own path/process rows.
- ⭐ **Favorites** — `Alt+f` pins a tab into the favorites block at the top of the sidebar.
- 🔎 **Search** — a search row above the tabs (focused at start, or `Ctrl+F`) filters tabs by path as you type.
- ⏵ **Replay** — a red `replay` button beside each process re-runs the shell's last command on click or `Alt+r`.
- 🎨 **Distinct tab colors** — a cohesive cool-jewel palette, each tab a different hue.
- 💾 **Session persistence** — reopens tabs, subshells, favorites and the active selection at their folders, replaying the command that was running in each shell.
- 🌿 **Git-aware status bar** — current path, active branch, and the app version pinned to the right corner.
- ✳️ **Live AI-agent model detection** — when [Claude Code](https://claude.com/claude-code) or [opencode](https://opencode.ai) runs in a tab, the status bar shows its currently selected model.
- 🖱️ **Mouse-native** — resize the sidebar, click to select tabs, drag to reorder, scroll terminal history.
- 🔍 **Scrollback** — 5000 lines per tab; wheel to scroll, any keystroke snaps back to live.
- 🪟 **Transparent** — inner apps (vim, mc, tmux…) keep full keyboard, modifier, mouse and bracketed-paste behavior.

## Install

```sh
cargo install --git https://github.com/riagentic/ricon
```

Or build from source (requires Rust 1.88+):

```sh
git clone https://github.com/riagentic/ricon
cd ricon
cargo build --release
./target/release/ricon
```

## Usage

```sh
ricon            # base path = current directory
ricon ~/code     # base path = given directory; the first shell starts here
                 # (new tabs/subshells inherit the active shell's directory)
```

### Shortcuts

| Key | Action |
| --- | --- |
| `Ctrl+t` / `Ctrl+n` | New tab |
| `Alt+s` | New subshell in the current tab |
| `Alt+Up` / `Alt+Down` | Previous / next shell within the tab |
| `Ctrl+w` | Close active shell (its tab closes with the last shell) |
| `Alt+f` | Toggle favorite (⭐, pinned to the top) |
| `Ctrl+f` | Focus the search row (filter tabs by path) |
| `Alt+r` | Replay the shell's last command |
| `Ctrl+q` | Quit gracefully |
| `Alt+1` … `Alt+9` | Select tab by number |
| `Alt+PgDn` / `Alt+PgUp` | Next / previous tab |
| Drag sidebar border | Resize sidebar |
| Click / drag a tab | Select tab & shell / reorder |
| Wheel over sidebar | Scroll the tab list |
| Wheel over pane | Scroll terminal history |

## How it works

- Each shell owns a PTY (`portable-pty`) fed into a `vt100` parser; the active shell renders through `tui-term`'s `PseudoTerminal`.
- A reader thread per shell pumps output and bumps an activity counter; the UI ticks at ~30 ms.
- The render path does no filesystem or `/proc` IO: cwd/process/agent facts are sampled at 2 Hz, the git branch at 500 ms, and the session persists at 1 Hz (with a final write on quit).
- AI-agent detection walks `/proc` for known agent processes descending from the tab's shell, then resolves the live model from the agent's own state (settings file, env, log tail, or opencode's SQLite store) — read-only, so the agent is never disturbed.
- Sessions persist to `$XDG_STATE_HOME/ricon/session` (or `~/.local/state/ricon/session`).

> **Platform:** Linux (agent detection and process inspection read `/proc`).

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
