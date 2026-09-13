# Application

## App basics
- provides linux terminal/shell functionality
- can have multiple tabs
- each new tab runs at least one shell
- each tab can have multiple shells
- can run at least one or multiple tabs
- tabs are vertical 
- tab states terminal number (starting by 1) and location (path) of the first shell
- new shell can be triggered by alt+t or alt+n
- all shortcuts of ricon's own are on alt (alt+t/alt+n new tab, alt+w close shell, alt+f search, alt+shift+f favorite, alt+q quit, alt+s subshell, alt+r replay, alt+c copy mode, alt+a select all, alt+? help); ctrl keys are never taken from the shell
- alt+? (or alt+h) shows a centered cheat sheet listing every shortcut; any key or click closes it and reaches nothing else
- tab can be selected also by alt+terminal number (for example alt+1 and alt+KP 1)
- terminal can be selected by clicking mouse on the tab 
- next tab can be activated by alt+PgDown
- previous tab can be activated by alt+PgUp
- the active tab is painted in one soft pastel color with dark text; inactive tabs have no background of their own (the terminal's colors)
- the status bar wears the same pastel as the active tab
- terminal doesn't break functionality of any application running in it
- app fills entire available space
- app lives in existing terminal
- base path is working directory from where the app was started
- base path is path derived from the first parameter (if such parameter is given)
- first shell (if it was not persisted)  start from the base path
- has status bar (footer) with `tab index/tab count`, location (path)
- status bar shows location (path)
- status bar shows active branch (if folder is within git repository) 
- side panel width is resizable by mouse
- if shell is not visible (ie. enother tab shell is active) and shell output changes, there will be `*` char shown after location on the first row of the tab
- terminal output is scrollable
- tab list is scrollable if its height exceeds available area height 
- up-down arrow char (scoll indicator) is shown on the footer before active tab index when not all tabs are visible
- mouse wheel can be used to scroll the tab list when tab list exceeds available area height and mouse cursor is in tab area

## Mouse
- specific shell of specific tab can is selected when clicked by mouse

## Tab rows
- first tab row shows current folder name, this is tab name
- second tab row shows the location (full) path
- third row of tab contains currently running process name
- last row of the tab separates the tabs: empty on an inactive tab; on the active tab it is an activity bar — a track across the panel with a lit segment sweeping left to right along it while any shell of the tab produces output (the same window as the spinner), the plain track while it is quiet
- if shell output changes, on third row after process name, white asci spinner indicating activity using brail code character rotated in 0.5 rps speed that lasts 1 seconds after and is removed after that is shown (on the active tab's pastel the spinner takes the tab's dark text color, since white would vanish there)

## Shell information within the tab
- in a multi-shell tab, the active shell's information (path, process) is bold (a single-shell tab is distinguished by the tab marker alone)

## Persistance
- app persists all open tabs and their folder locations
- app opens with tabs tabs as they were persisted (location and proces tree)
- app persists currently executed command(s) for each tab so it can restored them when when app restarts
- app persists last active tab and restors it after app restart
- app persist active shell within each tab if there are multiple shells for the tab
- favorite tabs are persisted
- active shells are persisted
- active tab is persisted

## Shortcuts
- Alt+c switches copy mode on / off (select console text over an app that owns the mouse)
- Alt+a selects the whole visible screen and copies it
- Alt+q opens a centered confirmation dialog "Are you sure to Quit all tabs?" with YES and NO buttons
- the quit dialog preselects NO; arrows toggle YES/NO, Enter confirms, Esc cancels — only YES quits the app gracefully
- the quit dialog is modal: keys, mouse and paste don't reach the shells while it is open
- every ricon shortcut is a bare Alt chord: Ctrl+Alt chords (Emacs/readline C-M- bindings) and every Ctrl key reach the shell

## Tab
- first row of the tab is current folder name 
- next rows are the shell information
- last row is empty as a separateor between tabs
- active tab shows `▶` 
- active shell shows `▶` as well with two prefix spaces are added

## Tabs
- order of tabs can be changed by dragging the tab using mouse
- new tab is opened right after the active tab or after last favorite tab, whatever comes later
- new tab working directory inherits the active tab's working directory (falling back to the base/default location when it can't be read)

## Shells
- each tab can hold multiple shells
- each tab shells are connected to the tab and cannot be moved independently
- shell is created when shell is active and using Alt+s shortcut
- bshell shares the same color as the parent shell
- navigation within one shell with shell can be done using Alt+Down or Atl+Up
- each shell adds two rows to the tab showing path and running process
- shell text is bold when it is the active shell among a tab's multiple shells
- if tab has multiple hells, Alt+w only closes active shell
- subshells are persisted

## Favorites
- tab can be marked and umarked as favorite using Alt+Shift+f
- when tab is marked as favorite, there is `⭐` added before tab name
- when tab is marked as favorite it changes position and going on top after last existing favorite tab

## Active tab
- active tab lines have "│" as the first character (the empty last line included)


## Replay button
- when process is `bash` or any other shell, app captures last command that is typed and confirmed with Enter to save it as a command for replay. Replay perfors typing the same text into console and executing it with Enter
- while a shell is the foreground process, a `replay` row sits just below the process row: the emoji, then the full command that will be replayed — the emoji is clickable / Alt+r; the whole row is hidden while any other program is in the foreground


## Copy text to clipboard
- text can be selected by mouse in a way that only terminal text is selected but not the tabs
- selecting a range and releasing copies it to the host clipboard via OSC 52 — works over SSH; multi-line copies keep hard line breaks and rejoin soft-wrapped lines; trailing blank space is dropped
- double-click copies the word under the cursor, triple-click the whole line, Alt+a the whole visible screen
- copy mode (the status-bar button, or Alt+c) selects console text even over an app that grabbed the mouse (a coding agent, vim, less, htop); with it off, Alt+drag does the same in one gesture and a plain drag is forwarded to that app unchanged
- a brief "✓ copied" confirmation flashes in the footer after a copy

