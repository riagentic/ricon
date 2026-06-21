# ricon

A fast terminal console with **vertical tabs** — built with [ratatui](https://ratatui.rs) + [portable-pty](https://crates.io/crates/portable-pty). Each tab is a real shell; the sidebar shows folder, path, running process, git branch and live activity. ricon lives inside your existing terminal and aims to be the best console with vertical tabs.

```
┌ ricon ──┐
│▶1 ricon │   $ cargo build --release
│   ~/code/gen/ricon
│   └ cargo ⠹
│         │
│ 2 web  *│
│   ~/code/web
│   └ vite │
└──────────┘ 1 ▸ ~/code/gen/ricon  ⎇ main  ✳ claude-opus-4-8     v0.1.0
```

## Features

- 🗂️ **Vertical tabs** — one shell per tab; 4-row entries show folder, full path, running process + activity spinner.
- 🎨 **Distinct tab colors** — a cohesive cool-jewel palette, each tab a different hue.
- 💾 **Session persistence** — reopens your tabs at their folders and replays the command that was running in each.
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
ricon ~/code     # base path = given directory; every new tab starts here
```

### Shortcuts

| Key | Action |
| --- | --- |
| `Ctrl+t` / `Ctrl+n` | New shell tab |
| `Ctrl+w` | Close current tab |
| `Ctrl+q` | Quit gracefully |
| `Alt+1` … `Alt+9` | Select tab by number |
| `Alt+PgDn` / `Alt+PgUp` | Next / previous tab |
| Drag sidebar border | Resize sidebar |
| Click / drag a tab | Select / reorder |
| Wheel over pane | Scroll terminal history |

## How it works

- Each tab owns a PTY (`portable-pty`) fed into a `vt100` parser; the active tab renders through `tui-term`'s `PseudoTerminal`.
- A reader thread per tab pumps output and bumps an activity counter; the UI ticks at ~30 ms.
- AI-agent detection walks `/proc` for known agent processes descending from the tab's shell, then resolves the live model from the agent's own state (settings file, env, log tail, or opencode's SQLite store) — read-only, so the agent is never disturbed.
- Sessions persist to `$XDG_STATE_HOME/ricon/session` (or `~/.local/state/ricon/session`).

> **Platform:** Linux (agent detection and process inspection read `/proc`).

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
