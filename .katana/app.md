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
- every new shell start from the base path
- has status bar (footer) with location (path)
- status bar shows location (path)
- status bar shows active branch (if folder is within git repository) 
- side panel width is resizable by mouse
- each tab takes four rows
- if shell is not visible (ie. enother tab shell is active) and shell output changes, there will be `*` char shown after location on the first row of the tab
- terminal output is scrollable
- tab list is scrollable if its height exceeds available area height 

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
