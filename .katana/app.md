# Application

## App basics
- provides linux terminal/shell functionality
- can have multiple tabs
- each new tab runs at least one shell
- each tab can have multiple shells
- can run at least one or multiple tabs
- tabs are vertical 
- tab states terminal number (starting by 1) and location (path) of the first shell
- new shell can be triggered by ctrl+t or ctrl+n
- tab can be selected also by alt+terminal number (for example alt+1 and alt+KP 1)
- terminal can be selected by clicking mouse on the tab 
- next tab can be activated by alt+PgDown
- previous tab can be activated by alt+PgUp
- every terminal tab has different aesthetical colors
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
- last row of the tab is empty separating the tabs 
- if shell output changes, on third row after process name, white asci spinner indicating activity using brail code character rotated in 0.5 rps speed that lasts 1 seconds after and is removed after that is shown
- fourth row is empty

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
- Ctrl+q quits the app gracefully

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
- if tab has multiple hells, ctrl+w only closes active shell
- subshells are persisted

## Favorites
- tab can be marked and umarked as favorite using Alt+f
- when tab is marked as favorite, there is `⭐` added before tab name
- when tab is marked as favorite it changes position and going on top after last existing favorite tab

## Active tab
- active tab lines have "│" as the first character (the empty last line included)


## Replay button
- when process is `bash` or any other shell, app captures last command that is typed and confirmed with Enter to save it as a command for replay. Replay perfors typing the same text into console and executing it with Enter
- next to process name, there is emoji representing `replay` — shown (and clickable / Alt+r) only while the process is a shell; hidden while any other program is in the foreground
