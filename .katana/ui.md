# UI
- tab bar resize doesn't trigger output updated notification aka `*`
- tab bar resize doesn't trigger output change animation


## Selection & clipboard
- left-drag in the terminal pane selects text; the selected cells are shown reversed
- releasing the drag copies the selection to the host clipboard via OSC 52
- a plain click (no drag) in the pane clears any selection
- scrolling or resizing the pane clears the selection (its grid coordinates go stale)


## Search row
- before all tabs, there is one search line that filters tabs based on input
- search row can be selected by mouse
- search row can be selected by Ctrl+F
- search row has focus when app is started so after app start user can type in search immediately
- when tabs are filtered, tab-switching (Alt+PageUp/Down, Alt+1–9) considers only visible (filtered) tabs

