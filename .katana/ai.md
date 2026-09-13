# AI

## Claudecode integration
- if claude code is running within the shell, status bar shows model name on the status bar
- the model shown is the one *this* tab's session is on: claude code registers each running session under `~/.claude/sessions/<pid>.json`, and that names the transcript to read — two sessions in one project each show their own model; without a registration the newest transcript of the project is read; a registered session whose transcript does not exist yet shows no model from it and no usage — the newest file is another session's
- the status bar also shows the session's context usage as `used/max` (`93k/1M`): the last answer's prompt plus cache written and read, against the model's window (1M for a `[1m]` model, else 200k — and 1M whenever the usage itself is past 200k)

## Opencode integration
- if opencode is running within the shell, status bar shows model name on the status bar
- the status bar also shows the session's context usage: the token total of the last answer in opencode's message store, against the model's context limit from opencode's model catalog cache when it has one

## Openclaude integration
- if openclaude is running within the shell, status bar shows model name on the status bar

## Features for all supported ai clients (claude, openclaude, opencode)
- there is auto feature button (enabled = green background, disabled = gray background), disabled by default for new tabs and persisted, and it enables or disables the "continue" feature
- auto feature is for each tab, on the first line of the tab after its text and indicators, glued to the right edge of the tab panel
- when "continue" feature is enabled and supported ai client is running in the particular tab and the agent has been waiting for user input for 10 minutes, then ricon types and confirms `/compact` so the conversation is summarized first
- waiting is what the client itself reports where it does: claude code registers `status` (`idle`, else busy) and `statusUpdatedAt` under `~/.claude/sessions/<pid>.json`, and that is exact; a client reporting nothing (opencode) is waiting when the visible console content is unchanged — bytes that repaint the same screen (cursor blink, redraw ticks) are not activity
- the wait never runs past the user's last keystroke into that shell, nor past the auto feature's own last nudge: typing into the tab defers the nudge
- proactive compaction: with the context past 70% of its window, an agent waiting for 60 seconds is compacted at once (the same two steps), and not again within 10 minutes — the usage read is the last answer's and only drops after the agent has answered again
- with the auto feature on, the status bar counts down to the nudge (`⏳ 7:12 → /compact`) once the agent has been waiting 30 seconds; while a nudge is under way it shows `⟳ /compact`, then `⟳` until the agent answers
- the context usage in the status bar is painted red from 80% of the window
- switching the auto feature off abandons a nudge under way (its mark included); it is never resumed later
- a client's `idle` stamped before the user's last keystroke or the last nudge is stale (background shells are probed only every few seconds): until the probe catches up, the screen stands in
- an agent that goes idle in a shell that is not on screen pings the host terminal once per idle stretch (a bell, then OSC 9, OSC 777 and OSC 99 notifications) — 3 seconds after the client reports idle, or a minute of still screen without a status; an idle stretch seen on screen is never rung for later
- when the compaction is done — the client reports idle again since the command, or the screen changed after the command and has then stood still for 15 seconds, or 5 minutes passed without any change — ricon types and confirms the auto-continue-text
- auto-continue-text is the content of `.ai/auto.md` in the project (the agent's working directory), else of `$XDG_CONFIG_HOME/ricon/auto.md` (`~/.config/ricon/auto.md`), trimmed, whichever exists and is not blank first; otherwise it is "continue"
- both steps are inserted and confirmed only when auto feature is enabled for particular tab and the agent itself holds the tty (its process group is the terminal's foreground group — an agent suspended under a nested shell does not), otherwise nothing is happening; an agent gone between the two steps — or a user who typed into the tab since the compact — gets no second one
- state of `auto` button is persisted so it survives ricon app restart
- both `/compact` and the auto-continue text are not only typed but confirmed (same as if user press Enter)

## Session transcripts
- while a supported ai client (claude, openclaude, opencode) runs in a shell, ricon appends that console's text to a dated file, so the session survives a power cut and can be fed back to the agent as lost context
- transcripts live in `$RICON_TRANSCRIPTS`, else `$XDG_DATA_HOME/ricon/sessions`, else `~/.local/share/ricon/sessions`, in a `YYYY-MM-DD` directory, one file per agent session named `HHMMSS-<client>-<folder>-<pid>.txt`; the transcript closes when the agent leaves the shell, and a later agent in the same shell gets a file of its own
- `RICON_TRANSCRIPTS=off` (or `0`, `no`, `false`, empty) writes no transcripts at all
- the file is plain console text — what the terminal showed, not the escape sequences that drew it — with a header naming the client, the model and the working directory
- a line is written once and never twice, and never stops being written — also once the shell's scrollback is full; a dated marker separates blocks written more than a minute apart
- the last screen, which has not scrolled off yet, is kept in a `<name>.tail.txt` sidecar beside the transcript and folded into it (sidecar removed) when the shell or the app closes
- a client holding the alternate screen (its chat scrolls inside it, never into the scrollback) is recorded as screen snapshots of settled frames instead — at most one every two seconds, and only when the screen changed
- transcribing costs the render thread nothing: the file is opened and written by the shell's own threads
- a shell with no ai client in it is never transcribed
