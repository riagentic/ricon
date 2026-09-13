# UI
- tab bar resize doesn't trigger output updated notification aka `*`
- tab bar resize doesn't trigger output change animation


## Selection & clipboard
- left-drag in the terminal pane selects text; the selected cells are painted in the selection color (never merely reversed — a reversal cancels itself out over text that is already inverse)
- the selection is confined to the pane grid (dragging into the sidebar clamps to its first column) — sidebar/tab text is never selected or copied
- the pane's first column starts a selection; the sidebar resize handle stays inside the sidebar
- double-click selects the word under the cursor and copies it — a word keeps the punctuation that holds paths, URLs, flags and identifiers together; triple-click takes the whole logical line, soft-wrapped continuations included
- releasing the drag copies the selection to the clipboard and to the primary selection
- Ctrl+C and Esc are never shadowed: they always reach the shell; with a selection live they take the highlight down on their way through (the release already copied)
- the copy goes out over both routes: OSC 52 (ssh/tmux/kitty/wezterm) and the local X11/Wayland clipboard + primary selection (gnome-terminal and other VTE terminals ignore OSC 52)
- a middle click in the pane pastes the primary selection into the shell (bracketed when the app asked for bracketed paste), as every terminal does — ricon holds the mouse, so the host terminal cannot
- Alt+a selects the whole visible screen and copies it — "copy everything" is always drawn as a selection first, never taken behind the user's back
- a selection belongs to the shell it was made in: switching tab or shell drops it
- the wheel scrolls the pane and carries a live selection along with the lines it covers, so a selection can span more than one screenful; it is dropped once it has scrolled out of view
- a plain click (no drag) in the pane clears any selection; resizing the pane clears it (its grid coordinates go stale)

### Copy mode: selecting over an app that grabbed the mouse
- copy mode is on by default and is a setting, not a one-shot gesture: it is switched by the status-bar button or Alt+c, and the choice is persisted with the session (`#copy off` line)
- with copy mode on, over an app that grabbed the mouse (a coding agent, vim, less, htop, opencode) the pane's left button and middle button belong to ricon: a plain drag selects, double-click takes a word, a middle click pastes — while the wheel, the right button and motion still reach the app, so it keeps scrolling its own content
- with copy mode off, the whole mouse is forwarded to an app that grabbed it; Alt+drag then selects instead of forwarding (Shift+drag does too, but host terminals usually keep Shift for their own window-wide selection — which highlights whole rows, sidebar text included, so Alt is the modifier to reach for)
- over an app that did not grab the mouse the mode changes nothing: the pane's mouse is ricon's either way, and the wheel scrolls the scrollback
- copy mode takes no key from the shell — Esc and Ctrl+C always go through

### Status-bar button
- a button sits in the status bar flush left of the version; it reads `⧉ copy ✓` while copy mode is on (painted in the selection color) and `⧉ copy ✗` while it is off (a plain button)
- clicking it switches copy mode and persists the choice at once — it never copies anything itself
- the `✓ copied` hint lands beside the button, never over it; on a footer too narrow for both button and version the button is dropped, not squeezed


## Search row
- before all tabs, there is one search line that filters tabs based on input
- search row can be selected by mouse
- search row can be selected by Alt+f
- search row has focus when app is started so after app start user can type in search immediately
- when tabs are filtered, tab-switching (Alt+PageUp/Down, Alt+1–9) considers only visible (filtered) tabs

