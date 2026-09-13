# ricon

A fast terminal console with **vertical tabs** — built with [ratatui](https://ratatui.rs) + [portable-pty](https://crates.io/crates/portable-pty). Each tab holds one or more real shells; the sidebar shows folder, path, running process, git branch and live activity. ricon lives inside your existing terminal and aims to be the best console with vertical tabs.

```
┌ ricon ──┐
│▶1 ricon │   $ cargo build --release
│   ~/code/gen/ricon
│   └ cargo ⠹
│  ▶~/code/gen/ricon/src
│   └ vim
│──━━━━━━──│
│ 2 ⭐web *│
│   ~/code/web
│   └ vite │
└──────────┘ 1/2 ▸ ~/code/gen/ricon  ⎇ main  ✳ claude-opus-5 93k/1M  ⧉ copy ✓ v0.4.0
```

## Features

- 🗂️ **Vertical tabs** — entries show folder, full path, running process + activity spinner.
- 🐚 **Subshells** — `Alt+s` adds extra shells to a tab; they move with it and add their own path/process rows.
- ⭐ **Favorites** — `Alt+Shift+f` pins a tab into the favorites block at the top of the sidebar.
- 🔎 **Search** — a search row above the tabs (focused at start, or `Alt+f`) filters tabs by path as you type.
- ⏵ **Replay** — a red `replay` button beside each process re-runs the shell's last command on click or `Alt+r`.
- 📋 **Select, copy & paste** — drag to select text in the terminal pane (pane content only, never the sidebar); releasing copies it to the system clipboard *and* the primary selection. Double-click takes the word under the cursor (a whole path or URL, not one segment), triple-click the whole line, `Alt+a` the whole visible screen. Middle-click pastes the primary selection, as in any terminal. The copy goes out both as OSC 52 (ssh/tmux/kitty/wezterm) and to the local X11/Wayland clipboard, so it also lands under gnome-terminal & friends, which ignore OSC 52. `Ctrl+c` and `Esc` are never taken from the shell.
- 🎯 **Copy mode** — on by default, shown as **⧉ copy ✓** in the status bar: even over an app that grabbed the mouse (a coding agent, vim, less, htop) a plain drag selects and a middle click pastes, while the wheel and the other buttons still reach the app. Click the button or press `Alt+c` to switch it off (**⧉ copy ✗**) and hand the whole mouse to the app — the choice is remembered. With it off, hold **Alt** while dragging for a one-off selection. (Most host terminals keep `Shift`+drag for their own selection, which spans the whole window — sidebar text included — so reach for copy mode or Alt instead.)
- 🎨 **One pastel, and an activity bar** — the active tab (and the status bar) wear a soft pastel with dark text; every other tab draws in the terminal's own colors. The active tab's last row is a bar with a lit segment sweeping along it while output streams, still while the tab is quiet.
- 💾 **Session persistence** — reopens tabs, subshells, favorites and the active selection at their folders, replaying the command that was running in each shell.
- 🌿 **Git-aware status bar** — current path, active branch, and the app version pinned to the right corner.
- ✳️ **Live AI-agent model and context** — when [Claude Code](https://claude.com/claude-code), openclaude or [opencode](https://opencode.ai) runs in a tab, the status bar shows the model that tab's session is on and how full its context is (`93k/1M`). The session is the tab's own — two sessions open in the same project each show their own model — and the model follows an in-session `/model` switch.
- ⟳ **Auto-continue** — with a tab's **⟳ auto** button on (off by default on a new tab; it sits at the right edge of the tab panel on the tab's first line — click it to toggle, green = on, gray = off, and the choice is remembered per tab), an AI agent that has been waiting for ten minutes — in any tab, not just the visible one — is first typed `/compact`, and once the compaction has settled, `continue` — or the text of the project's `.ai/auto.md`, else of `~/.config/ricon/auto.md`, when either exists. Waiting is what the agent itself reports where it does (Claude Code registers `idle`/`busy` with a timestamp); otherwise it is the console *content* standing still, so an agent repainting the same screen still counts. Typing into the tab defers the nudge, and typing during the compaction cancels the continue step; switching the button off abandons a nudge under way. Only ever typed while that agent itself holds the terminal, never at a shell prompt — and never invisibly: the status bar counts down to it (`⏳ 7:12 → /compact`), the tab's button turns yellow and the status bar shows `⟳ /compact`, then `⟳`, until that agent answers, and always for long enough to be seen.
- 🧹 **Proactive compaction** — past 70 % of the context window, an agent that has been idle for just a minute is compacted right away (once per ten minutes, since the usage the footer reads only drops after the agent's next answer). The usage turns red from 80 %.
- 🔔 **Idle ping** — an agent that goes idle in a tab that is not on screen rings the host terminal once: a bell plus the notification sequences terminals speak (OSC 9 for iTerm2/WezTerm, OSC 777 for foot/urxvt, OSC 99 for kitty), so the desktop tells you it is waiting.
- ❔ **Cheat sheet** — `Alt+?` (or `Alt+h`) lists every shortcut; any key or click closes it.
- 📝 **Session transcripts** — while [Claude Code](https://claude.com/claude-code), openclaude or [opencode](https://opencode.ai) runs in a tab, everything on that console is appended to a dated text file under `~/.local/share/ricon/sessions/YYYY-MM-DD/` — one per agent session, closed when the agent exits. A power cut, a crash or a closed tab then costs nothing: the file is the conversation, in plain text, ready to be pasted back into the agent as the context it lost. The text comes from ricon's own terminal emulator, so it reads like the console did — no escape sequences, no repaint noise. Set `RICON_TRANSCRIPTS` to another directory to move them, or to `off` to write none.
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
| `Alt+t` / `Alt+n` | New tab |
| `Alt+s` | New subshell in the current tab |
| `Alt+Up` / `Alt+Down` | Previous / next shell within the tab |
| `Alt+w` | Close active shell (its tab closes with the last shell) |
| `Alt+Shift+f` | Toggle favorite (⭐, pinned to the top) |
| `Alt+f` | Focus the search row (filter tabs by path) |
| `Alt+r` | Replay the shell's last command |
| `Alt+q` | Quit gracefully |
| `Alt+1` … `Alt+9` | Select tab by number |
| `Alt+PgDn` / `Alt+PgUp` | Next / previous tab |
| Drag sidebar border | Resize sidebar |
| Click / drag a tab | Select tab & shell / reorder |
| Wheel over sidebar | Scroll the tab list |
| Wheel over pane | Scroll terminal history |
| Drag in pane | Select pane text → clipboard (with copy mode off: `Alt`+drag over mouse-driven apps) |
| Double / triple click in pane | Select the word / the whole line, and copy it |
| Middle click in pane | Paste the primary selection |
| `Alt+c` / click `⧉ copy ✓` | Switch copy mode off (mouse-driven apps get the whole mouse) and back on |
| `Alt+a` | Select the whole visible screen and copy it |
| `Alt+?` / `Alt+h` | Cheat sheet with every shortcut |

Every shortcut of ricon's own is a bare `Alt` chord, so every `Ctrl` key — `Ctrl+c` included — and every `Ctrl+Alt` chord reaches the shell untouched.

## How it works

- Each shell owns a PTY (`portable-pty`) fed into a `vt100` parser; the active shell renders through `tui-term`'s `PseudoTerminal`.
- A reader thread per shell pumps output and bumps an activity counter, and a writer thread drains its input queue — so neither a firehose of output nor a paste into a program that has stopped reading can block the UI.
- The UI draws at most once per ~30 ms frame and handles every event already queued before drawing again, so a burst of input (mouse motion under an app in any-motion mode) costs one frame instead of one full render per event.
- The render path does no filesystem or `/proc` IO: cwd/process facts are sampled at 2 Hz, the git branch at 500 ms, and the session persists at 1 Hz (with a final write on quit). Agent detection — a `/proc`-wide scan plus a database read — runs on its own worker thread and posts its answer back.
- AI-agent detection walks `/proc` for known agent processes descending from the tab's shell, then resolves the live model from the agent's own state (its session transcript, settings file, env, log tail, or opencode's SQLite store) — read-only, so the agent is never disturbed. The transcript comes first: it is stamped with the model that actually answered, so an in-session model switch shows up, which frozen config and env cannot express. Which transcript is *this* tab's: Claude Code registers each running session under `~/.claude/sessions/<pid>.json`, and that file names it — so two sessions in one project never show each other's model; a client that registers nothing falls back to the newest transcript. Context usage is the last answer's own `usage` (prompt plus cache written and read — the conversation as the model saw it) against the model's window (1M for a `[1m]` model, else 200k); opencode's comes from its message store and its model catalog. Probes rotate over every shell (the on-screen one every other turn), so auto-continue sees agents in background tabs too.
- Idle is read from the client where it says so: Claude Code's `sessions/<pid>.json` carries `status` and `statusUpdatedAt`, so the wait is exact (and survives a ricon restart); anything but `idle` counts as busy, and a client that reports nothing falls back to the screen hash. The wait never runs past the user's last keystroke into the tab, nor past the auto feature's own last nudge.
- The auto feature's two steps — `/compact`, then the continue text — are each delivered the way a person delivers them: bracketed paste (when the client asked for that mode), a pause, then Enter as a separate write. Full-screen clients read their tty in chunks and take a many-byte chunk as pasted text, so a `\r` sent in the same chunk is a newline in the composer and the message never leaves — the pause is what makes the confirmation a keypress. The compaction counts as done once the client reports idle again since the command, or the screen changed after it and then stood still for 15 s (the client redraws a spinner while it summarizes), or after 5 minutes without any change.
- Transcripts are taken from the shell's parsed screen, not the raw PTY stream: lines are committed once they scroll off (the emulator has already resolved every repaint into a final line), and a full-screen client that scrolls its chat inside the alternate screen — where nothing ever reaches the scrollback — is snapshotted instead, on a settled frame, at most every 2 s and only when it changed. What is still on screen is mirrored to a `.tail` sidecar (atomically, then `fsync`ed) and folded into the transcript when the session closes; a transcript still sitting beside a `.tail` file is one whose ricon never got to close it, and the two together are the whole session. Writes happen on the shell's reader thread and on a 1 Hz flush thread — an agent waiting for an answer emits no bytes at all, and its answer is exactly what must already be on disk.
- Sessions persist to `$XDG_STATE_HOME/ricon/session` (or `~/.local/state/ricon/session`), each tab's auto-continue choice and the copy-mode switch included.
- Copies take both routes at once: an OSC 52 sequence for the host terminal (the only path that survives ssh) and the local desktop clipboard *and* primary selection via `arboard` (X11/Wayland), which is what makes copy work on VTE-based terminals that drop OSC 52. A middle-click paste reads the primary selection back the same way, on the clipboard's own thread (a read waits on the selection's owner), and types it into the shell as a bracketed paste when the app asked for that mode — ricon holds the mouse, so the host terminal cannot paste on its behalf.

> **Platform:** Linux (agent detection and process inspection read `/proc`).

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
