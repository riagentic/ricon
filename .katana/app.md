# Application

- provides linux shell functionality
- can run one or multiple shells, each having own tab
- shell tabs are vertical 
- shell tab states terminal number (starting by 1) and location (path) of the terminal
- new shell can be triggered by ctrl+t or ctrl+n
- terminal can be closed by using ctrl+w
- terminal can be selected also by alt+terminal number (for example alt+1 and alt+KP 1)
- terminal can be selected by clicking mouse on the tab 
- next terminal can be activated by alt+PgDown
- previous terminal can be activated by alt+PgUp
- every terminal tab has different aesthetical colors
- terminal doesn't break functionality of any application running in it
- app fills entire available space
- app lives in existing terminal
- base path is working directory from where the app was started
- base path is path derived from the first parameter (if such parameter is given)
- first shell (if it was not persisted)  start from the base path
- has status bar (footer) with tab index / tab count, location (path)
- status bar shows location (path)
- status bar shows active branch (if folder is within git repository) 
- side panel width is resizable by mouse
- each tab takes four rows
- if shell is not visible (ie. enother tab shell is active) and shell output changes, there will be `*` char shown after location on the first row of the tab
- terminal output is scrollable
- tab list is scrollable if its height exceeds available area height 
- up-down arrow char (scoll indicator) is shown on the footer before active tab index when not all tabs are visible
- mouse wheel can be used to scroll the tab list when tab list exceeds available area height and mouse cursor is in tab area

## Rows
- each tab takes four rows
- first tab row shows current folder name
- second tab row shows the location (full) path
- third row of tab contains currently running process name
- if shell output changes, on third row after process name, white asci spinner indicating activity using brail code character rotated in 0.5 rps speed that lasts 1 seconds after and is removed after that is shown
- fourth row is empty

## Persistance
- app persists all open tabs and their folder locations
- app opens with tabs tabs as they were persisted (location and proces tree)
- app persists currently executed command(s) for each tab so it can restored them when when app restarts
- app persists last active tab and restors it after app restart

## Shortcuts
- Ctrl+q quits the app gracefully

## Tabs
- order of tabs can be changed by dragging the tab using mouse
- new tab is opened right after the active tab
- new tab working directory is derived from the active tab

## Subshells
- each tab can hold multiple subshells
- each subshell is tighted to the parent shell and cannot be moved independently
- subshell is created when shell is active and using Alt+s shortcut
- subshell shares the same color as the parent shell
- navigation within one shell with subshell can be done using Alt+Down or Atl+Up
- each subshell adds two rows to the tab showing path and running process
- ubshell text is bold when subshell is active
- if tab has multiple subshells, ctrl+w only closes active subshell

## Favorites
- tab can be marked and umarked as favorite using Alt+F 
- when tab is marked as favorite, there is yellow ★ before tab name
- when tab is marked as favorite it changes position and going on top after last existing favorite tab
