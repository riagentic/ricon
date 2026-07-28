# UI
- tab bar resize doesn't trigger output updated notification aka `*`
- tab bar resize doesn't trigger output change animation


## Selection & clipboard
- left-drag in the terminal pane selects text; the selected cells are shown reversed
- the selection is confined to the pane grid (dragging into the sidebar clamps to its first column) — sidebar/tab text is never selected or copied
- over an app that grabbed the mouse (vim/less/htop), Alt+drag selects instead of forwarding; Shift+drag does too, but host terminals usually keep Shift for their own window-wide selection
- releasing the drag copies the selection to the clipboard
- Ctrl+C copies the live selection and clears it; with nothing selected Ctrl+C is forwarded to the shell as SIGINT, never shadowed
- the copy goes out over both routes: OSC 52 (ssh/tmux/kitty/wezterm) and the local X11/Wayland selection (gnome-terminal and other VTE terminals ignore OSC 52)
- a `⧉ copy` button sits in the status bar flush left of the version; clicking it copies the selection, or the whole visible screen when nothing is selected — the copy path that still works under an app owning the mouse and Ctrl+C (no app shortcut is ever redefined to get it)
- the copy button keeps the selection (idempotent), and the `✓ copied` hint lands beside it, never over it; on a footer too narrow for both button and version the button is dropped, not squeezed
- a plain click (no drag) in the pane clears any selection
- scrolling or resizing the pane clears the selection (its grid coordinates go stale)


## Search row
- before all tabs, there is one search line that filters tabs based on input
- search row can be selected by mouse
- search row can be selected by Ctrl+F
- search row has focus when app is started so after app start user can type in search immediately
- when tabs are filtered, tab-switching (Alt+PageUp/Down, Alt+1–9) considers only visible (filtered) tabs

