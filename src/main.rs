//! ricon — a console with vertical tabs (ratatui + portable-pty + vt100).

use std::{
    error::Error,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::{
        event::{
            self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
            Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
            MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
        },
        execute, terminal,
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use tui_term::{
    vt100::{self, MouseProtocolEncoding, MouseProtocolMode},
    widget::PseudoTerminal,
};

const SIDEBAR_WIDTH: u16 = 26;
const MIN_SIDEBAR_WIDTH: u16 = 8;
const MIN_PANE_WIDTH: u16 = 10;
const POLL_INTERVAL: Duration = Duration::from_millis(30);
/// Host-side scrollback retained per tab, and lines moved per wheel notch.
const SCROLLBACK_LINES: usize = 5000;
const SCROLL_STEP: usize = 3;

fn main() -> Result<(), Box<dyn Error>> {
    let mut terminal = ratatui::init();
    // Kitty keyboard protocol (where supported): hosts then report numpad keys
    // with full modifiers, so alt+numpad-digit works like alt+main-row-digit.
    let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let result = App::new().and_then(|mut app| app.run(&mut terminal));
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    result
}

// ── tab ──────────────────────────────────────────────────────────────────────

struct Shell {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    /// Output activity: bumped by the reader thread, consumed by the UI tick.
    activity: Arc<AtomicU64>,
    seen_activity: u64,
    spawned: Instant,
    last_change: Instant,
    /// Last PTY resize; shell repaints right after are not "new output".
    resized: Instant,
    animating: bool,
    /// Output arrived while this tab was not the active one; cleared on focus.
    unseen_output: bool,
    /// AI coding agent detected in this tab's shell, sampled periodically.
    agent: Option<AgentInfo>,
    /// Last time agent detection ran — throttles the /proc scan whether or not
    /// an agent was found (a missing agent must not re-scan every frame).
    agent_sampled: Instant,
    /// Restored command to replay once the shell's first prompt is up.
    pending_cmd: Option<String>,
}

struct AgentInfo {
    model: String,
}

/// A detectable AI coding agent: the process name to look for, plus an ordered
/// list of places its current model can be read from (first hit wins; if none
/// resolve, the agent's own name is shown as the label).
struct AgentSpec {
    comm: &'static str,
    sources: &'static [Source],
}

/// Where a model name can be read from, resolved in declaration order.
enum Source {
    /// A `$HOME`-relative JSON file and the key holding the model string.
    Settings(&'static str, &'static str),
    /// An env var whose value is JSON, and the key holding the model string.
    EnvJson(&'static str, &'static str),
    /// An env var whose value is the model string directly.
    EnvPlain(&'static str),
    /// The last `key=value` in the newest `*.log` of a `$HOME`-relative dir —
    /// reflects live in-session model switches that frozen env/config miss.
    LogTail(&'static str, &'static str),
    /// opencode's per-directory selected model, read from its state store and
    /// keyed by the agent process's cwd — this is what the opencode TUI shows.
    OpencodeSelected,
}

const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        comm: "claude",
        sources: &[Source::Settings(".claude/settings.json", "model"), Source::EnvPlain("ANTHROPIC_MODEL")],
    },
    AgentSpec {
        comm: "opencode",
        sources: &[
            // Per-directory live selection (what the TUI shows) is authoritative;
            // then the explicit launch model; then the last logged request; then
            // static config / env as last resorts.
            Source::OpencodeSelected,
            Source::EnvJson("OPENCODE_CONFIG_CONTENT", "model"),
            Source::LogTail(".local/share/opencode/log", "llm.model"),
            Source::Settings(".config/opencode/opencode.jsonc", "model"),
            Source::EnvPlain("OPENCODE_MODEL"),
        ],
    },
];

impl Shell {
    fn spawn(
        rows: u16,
        cols: u16,
        cwd: &Path,
        pending_cmd: Option<String>,
    ) -> Result<Self, Box<dyn Error>> {
        let pair = native_pty_system().openpty(pty_size(rows, cols))?;

        let mut cmd = CommandBuilder::new(default_shell());
        cmd.env("TERM", "xterm-256color");
        cmd.cwd(cwd);
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));
        let activity = Arc::new(AtomicU64::new(0));
        let mut reader = pair.master.try_clone_reader()?;
        let feed = Arc::clone(&parser);
        let pulse = Arc::clone(&activity);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                feed.lock().unwrap_or_else(PoisonError::into_inner).process(&buf[..n]);
                pulse.fetch_add(1, Ordering::Relaxed);
            }
        });

        Ok(Self {
            parser,
            writer: pair.master.take_writer()?,
            pid: child.process_id(),
            master: pair.master,
            child,
            activity,
            seen_activity: 0,
            spawned: Instant::now(),
            last_change: Instant::now(),
            resized: Instant::now(),
            animating: false,
            unseen_output: false,
            agent: None,
            agent_sampled: Instant::now(),
            pending_cmd,
        })
    }

    /// Replay a restored command once the shell has produced its first prompt
    /// (signalled by any output, plus a small settle), then forget it.
    fn flush_pending(&mut self) {
        const STARTUP: Duration = Duration::from_millis(250);
        let ready = self.activity.load(Ordering::Relaxed) > 0 && self.spawned.elapsed() > STARTUP;
        if ready && let Some(cmd) = self.pending_cmd.take() {
            let mut line = cmd.into_bytes();
            line.push(b'\r');
            let _ = self.writer.write_all(&line);
            let _ = self.writer.flush();
        }
    }

    /// Full command line of the process occupying this tab's foreground, when
    /// something other than the shell is running — what restore replays.
    fn foreground_cmd(&self) -> Option<String> {
        let fg = foreground_pid(self.pid?)?;
        let raw = std::fs::read(format!("/proc/{fg}/cmdline")).ok()?;
        let cmd = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(" ");
        (!cmd.is_empty()).then_some(cmd)
    }

    /// Detect a known AI coding agent (Claude Code, opencode) in this tab's
    /// shell and resolve its model name. Sampled at 2 Hz, re-resolved every
    /// sample so a model change via `/model` (which rewrites the settings file)
    /// shows up live; the process environment is only a fallback since it is
    /// frozen at exec time.
    fn tick_agent(&mut self) {
        const SAMPLE_EVERY: Duration = Duration::from_millis(500);
        // Throttle whether or not an agent is present: detection scans /proc,
        // which is far too costly to repeat every frame (e.g. during a resize).
        if self.agent_sampled.elapsed() < SAMPLE_EVERY {
            return;
        }
        self.agent_sampled = Instant::now();
        let Some(shell) = self.pid else {
            self.agent = None;
            return;
        };
        self.agent = detect_agent(shell).map(|(spec, pid)| {
            let model = spec
                .sources
                .iter()
                .find_map(|src| resolve_source(src, pid))
                .unwrap_or_else(|| spec.comm.to_string());
            AgentInfo { model }
        });
    }

    /// Advance the activity animation: the phase moves whenever output
    /// arrived since the last tick; it stops shortly after output settles.
    /// Output arriving while inactive sets the unseen marker; focus clears it.
    fn tick_activity(&mut self, is_active: bool) {
        // Rule: the animation lasts one more second after output settles.
        const SETTLE: Duration = Duration::from_secs(1);
        // Output within this window after a PTY resize is the shell repainting
        // its prompt on SIGWINCH, not real activity — it must trigger neither
        // the `*` marker nor the spinner animation.
        const RESIZE_GRACE: Duration = Duration::from_secs(1);
        let now = self.activity.load(Ordering::Relaxed);
        if now != self.seen_activity {
            self.seen_activity = now;
            if self.resized.elapsed() > RESIZE_GRACE {
                self.last_change = Instant::now();
                if !is_active {
                    self.unseen_output = true;
                }
            }
        }
        if is_active {
            self.unseen_output = false;
        }
        self.animating = self.last_change.elapsed() < SETTLE;
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(pty_size(rows, cols));
        self.parser.lock().unwrap_or_else(PoisonError::into_inner).screen_mut().set_size(rows, cols);
        self.resized = Instant::now();
    }

    /// Current working directory of the shell, read live from /proc.
    fn cwd(&self) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/cwd", self.pid?)).ok()
    }

    /// Name of the process currently running in this terminal: the foreground
    /// process group of the PTY, falling back to the shell itself.
    fn running_process(&self) -> String {
        let Some(pid) = self.pid else { return "?".into() };
        let fg = foreground_pid(pid).unwrap_or(pid);
        proc_comm(fg).or_else(|| proc_comm(pid)).unwrap_or_else(|| "?".into())
    }

    /// Move the host scrollback view by `delta` lines (positive = into older
    /// output); vt100 clamps to the buffer. `delta == 0` snaps back to live.
    fn scroll(&self, delta: isize) {
        let mut parser = self.parser.lock().unwrap_or_else(PoisonError::into_inner);
        let screen = parser.screen_mut();
        let at = if delta == 0 { 0 } else { (screen.scrollback() as isize + delta).max(0) as usize };
        screen.set_scrollback(at);
    }

    /// Snapshot of the input-relevant terminal modes the inner app has set.
    fn modes(&self) -> TermModes {
        let parser = self.parser.lock().unwrap_or_else(PoisonError::into_inner);
        let screen = parser.screen();
        TermModes {
            app_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
            mouse_mode: screen.mouse_protocol_mode(),
            mouse_encoding: screen.mouse_protocol_encoding(),
        }
    }
}

// ── tab: a shell plus its subshells ────────────────────────────────────────────

/// A terminal tab: one parent shell and zero or more subshells, each its own
/// PTY. The subshells live and move with the tab (they can't be reordered on
/// their own) and share its color. Exactly one shell is active — shown in the
/// pane and receiving input; the rest keep running in the background.
struct Tab {
    shells: Vec<Shell>,
    active: usize,
    color: Color,
}

impl Tab {
    /// A new tab: one parent shell in `cwd`, replaying `pending_cmd` if any.
    fn spawn(
        rows: u16,
        cols: u16,
        cwd: &Path,
        color: Color,
        pending_cmd: Option<String>,
    ) -> Result<Self, Box<dyn Error>> {
        let parent = Shell::spawn(rows, cols, cwd, pending_cmd)?;
        Ok(Self { shells: vec![parent], active: 0, color })
    }

    fn active_shell(&self) -> &Shell {
        &self.shells[self.active]
    }

    fn active_shell_mut(&mut self) -> &mut Shell {
        &mut self.shells[self.active]
    }

    /// Working directory of the active shell — what new tabs/subshells inherit.
    fn cwd(&self) -> Option<PathBuf> {
        self.active_shell().cwd()
    }

    /// Sidebar height: four rows for the parent, plus two per subshell.
    fn rows(&self) -> u16 {
        4 + 2 * (self.shells.len() as u16 - 1)
    }

    /// Cycle the active shell by `delta` (wrapping); a no-op with no subshells.
    fn navigate(&mut self, delta: isize) {
        let n = self.shells.len() as isize;
        self.active = (self.active as isize + delta).rem_euclid(n) as usize;
    }

    /// Resize every shell in the tab to the current pane size.
    fn resize(&mut self, rows: u16, cols: u16) {
        for shell in &mut self.shells {
            shell.resize(rows, cols);
        }
    }
}

/// Inner-app terminal modes that change how input must be encoded.
#[derive(Clone, Copy)]
struct TermModes {
    app_cursor: bool,
    bracketed_paste: bool,
    mouse_mode: MouseProtocolMode,
    mouse_encoding: MouseProtocolEncoding,
}

// ── app ──────────────────────────────────────────────────────────────────────

struct App {
    tabs: Vec<Tab>,
    active: usize,
    created: usize,
    pty_rows: u16,
    pty_cols: u16,
    term_width: u16,
    sidebar_width: u16,
    dragging_sidebar: bool,
    /// Tab currently being dragged to a new position in the sidebar.
    dragging_tab: Option<usize>,
    /// First tab visible in the (scrolling) sidebar, captured each render so a
    /// click maps to the right tab when the list is scrolled past tab 0.
    list_offset: usize,
    /// Sidebar height in rows, captured each render — sizes the tab viewport
    /// for wheel scrolling, overflow detection, and revealing the active tab.
    sidebar_rows: u16,
    /// Active tab last revealed into view; lets the active tab scroll into
    /// view when it changes without yanking the view back during free wheel
    /// scrolling (which leaves the active tab untouched).
    shown_active: usize,
    /// Working directory the app was started from; every new shell starts here.
    base: PathBuf,
    /// Last session (folder + command per tab) written to disk; persisted on change.
    saved_session: Vec<TabState>,
    /// Set by Ctrl+q; the loop then shuts every shell down and exits.
    quit: bool,
}

/// One persisted tab: its folder, the command running in it (if any), and
/// whether it was the active tab at save time.
#[derive(Clone, PartialEq)]
struct TabState {
    cwd: PathBuf,
    cmd: Option<String>,
    active: bool,
}

impl App {
    fn new() -> Result<Self, Box<dyn Error>> {
        let base = base_path(std::env::args().nth(1))?;
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            created: 0,
            pty_rows: 24,
            pty_cols: 80,
            term_width: 80,
            sidebar_width: SIDEBAR_WIDTH,
            dragging_sidebar: false,
            dragging_tab: None,
            list_offset: 0,
            sidebar_rows: 0,
            shown_active: 0,
            base,
            saved_session: Vec::new(),
            quit: false,
        };
        // Restore the persisted tabs at their saved folders, replaying each
        // tab's recorded command; fall back to a single base-path shell when
        // there is no (still-valid) session.
        let restored: Vec<TabState> = load_session().into_iter().filter(|s| s.cwd.is_dir()).collect();
        if restored.is_empty() {
            app.open_tab()?;
        } else {
            let active = restored.iter().position(|s| s.active).unwrap_or(0);
            for state in &restored {
                app.open_tab_at(&state.cwd, state.cmd.clone())?;
            }
            app.active = active.min(app.tabs.len().saturating_sub(1));
        }
        Ok(app)
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<(), Box<dyn Error>> {
        loop {
            self.reap_dead_tabs();
            if self.quit {
                // Graceful exit: stop every shell, then unwind to main() which
                // restores the host terminal (mouse, paste, keyboard flags).
                for tab in &mut self.tabs {
                    for shell in &mut tab.shells {
                        let _ = shell.child.kill();
                    }
                }
                return Ok(());
            }
            if self.tabs.is_empty() {
                return Ok(());
            }
            // Reveal the active tab into view only when it just changed — free
            // wheel scrolling (which never moves `active`) is left untouched.
            if self.active != self.shown_active {
                self.reveal_active();
                self.shown_active = self.active;
            }
            let active_tab = self.active;
            for (ti, tab) in self.tabs.iter_mut().enumerate() {
                let shown = tab.active;
                for (si, shell) in tab.shells.iter_mut().enumerate() {
                    // Only the active tab's active shell is on screen; every
                    // other shell's output is "unseen" until it is focused.
                    shell.tick_activity(ti == active_tab && si == shown);
                    shell.tick_agent();
                    shell.flush_pending();
                }
            }
            self.persist_session();
            self.fit_ptys(terminal.size()?.into());
            terminal.draw(|frame| draw(frame, self))?;

            if !event::poll(POLL_INTERVAL)? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key)?,
                Event::Mouse(mouse) => self.on_mouse(mouse)?,
                Event::Paste(text) => self.on_paste(&text)?,
                _ => {}
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<(), Box<dyn Error>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('q') if ctrl => self.quit = true,
            KeyCode::Char('t' | 'n') if ctrl => self.open_tab()?,
            KeyCode::Char('w') if ctrl => self.close_active(),
            KeyCode::Char('s') if alt => self.open_subshell()?,
            KeyCode::Up if alt => self.tabs[self.active].navigate(-1),
            KeyCode::Down if alt => self.tabs[self.active].navigate(1),
            KeyCode::Char(c @ '1'..='9') if alt => {
                let index = c as usize - '1' as usize;
                if index < self.tabs.len() {
                    self.active = index;
                }
            }
            KeyCode::PageDown if alt => self.active = (self.active + 1) % self.tabs.len(),
            KeyCode::PageUp if alt => {
                self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
            }
            _ => {
                let modes = self.tabs[self.active].active_shell().modes();
                if let Some(bytes) = encode_key(&key, modes.app_cursor) {
                    self.write_active(&bytes)?;
                }
            }
        }
        Ok(())
    }

    fn on_paste(&mut self, text: &str) -> Result<(), Box<dyn Error>> {
        if self.tabs[self.active].active_shell().modes().bracketed_paste {
            self.write_active(format!("\x1b[200~{text}\x1b[201~").as_bytes())
        } else {
            self.write_active(text.as_bytes())
        }
    }

    /// Sidebar rows available for tabs (row 0 is the " ricon " title).
    fn viewport_rows(&self) -> usize {
        self.sidebar_rows.saturating_sub(1) as usize
    }

    /// Total rows every tab would occupy — tabs have variable height (four
    /// rows plus two per subshell), so this is a sum, not a count × 4.
    fn content_rows(&self) -> usize {
        self.tabs.iter().map(|t| t.rows() as usize).sum()
    }

    /// Tab index under sidebar row `row`: walk tab heights from the current
    /// scroll offset. `None` for the title row (row 0) or rows past the last
    /// tab. Bounds against `tabs.len()` are the caller's to check.
    fn tab_at_row(&self, row: u16) -> Option<usize> {
        let mut r = (row as usize).checked_sub(1)?;
        for i in self.list_offset..self.tabs.len() {
            let h = self.tabs[i].rows() as usize;
            if r < h {
                return Some(i);
            }
            r -= h;
        }
        None
    }

    /// Largest first-visible index that still fills the viewport — the clamp
    /// for any scroll. Zero when every tab already fits.
    fn max_offset(&self) -> usize {
        let vp = self.viewport_rows();
        let mut used = 0;
        let mut i = self.tabs.len();
        while i > 0 && used + self.tabs[i - 1].rows() as usize <= vp {
            used += self.tabs[i - 1].rows() as usize;
            i -= 1;
        }
        i
    }

    /// Not every tab fits — i.e. the list is scrollable (drives both the wheel
    /// and the footer scroll indicator).
    fn tabs_overflow(&self) -> bool {
        self.content_rows() > self.viewport_rows()
    }

    /// Scroll the tab list by `delta` tabs (negative = toward the top),
    /// clamped to the scrollable range.
    fn scroll_tabs(&mut self, delta: isize) {
        self.list_offset = (self.list_offset as isize + delta).clamp(0, self.max_offset() as isize) as usize;
    }

    /// Bring the active tab into view, scrolling the minimum amount; a no-op
    /// when it is already visible.
    fn reveal_active(&mut self) {
        if self.active < self.list_offset {
            self.list_offset = self.active;
            return;
        }
        // Scroll down just enough that the active tab's last row is on screen.
        let vp = self.viewport_rows();
        while self.list_offset < self.active {
            let used: usize =
                self.tabs[self.list_offset..=self.active].iter().map(|t| t.rows() as usize).sum();
            if used <= vp {
                break;
            }
            self.list_offset += 1;
        }
    }

    /// Sidebar-border drags resize the panel; everything else over the
    /// terminal pane is forwarded to the inner app (when it asked for mouse).
    fn on_mouse(&mut self, mouse: MouseEvent) -> Result<(), Box<dyn Error>> {
        let border = self.sidebar_width.saturating_sub(1);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if mouse.column.abs_diff(border) <= 1 => {
                self.dragging_sidebar = true;
                return Ok(());
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_sidebar => {
                let max = self.term_width.saturating_sub(MIN_PANE_WIDTH).max(MIN_SIDEBAR_WIDTH);
                self.sidebar_width = (mouse.column + 1).clamp(MIN_SIDEBAR_WIDTH, max);
                return Ok(());
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging_sidebar => {
                self.dragging_sidebar = false;
                return Ok(());
            }
            // Click on a tab entry (row 0 is the title; four rows per tab)
            // selects that terminal and arms it for drag-reordering.
            MouseEventKind::Down(MouseButton::Left) if mouse.column < self.sidebar_width => {
                if let Some(index) = self.tab_at_row(mouse.row)
                    && index < self.tabs.len()
                {
                    self.active = index;
                    self.dragging_tab = Some(index);
                }
                return Ok(());
            }
            // Dragging a tab over another row reorders it to that position,
            // carrying the active selection with it.
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_tab.is_some() => {
                let from = self.dragging_tab.unwrap_or(0);
                if let Some(to) = self.tab_at_row(mouse.row)
                    && from < self.tabs.len()
                    && to < self.tabs.len()
                    && to != from
                {
                    let tab = self.tabs.remove(from);
                    self.tabs.insert(to, tab);
                    self.active = to;
                    self.dragging_tab = Some(to);
                }
                return Ok(());
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging_tab.is_some() => {
                self.dragging_tab = None;
                return Ok(());
            }
            // Wheel over the sidebar scrolls the tab list (one tab per notch)
            // when it overflows the viewport; over the pane it scrolls output.
            MouseEventKind::ScrollUp if mouse.column < self.sidebar_width => {
                self.scroll_tabs(-1);
                return Ok(());
            }
            MouseEventKind::ScrollDown if mouse.column < self.sidebar_width => {
                self.scroll_tabs(1);
                return Ok(());
            }
            _ => {}
        }
        if mouse.column >= self.sidebar_width && mouse.row < self.pty_rows {
            let modes = self.tabs[self.active].active_shell().modes();
            // Wheel over the pane scrolls this shell's scrollback — unless the
            // inner app grabbed the mouse, in which case it's forwarded.
            if modes.mouse_mode == MouseProtocolMode::None {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.tabs[self.active].active_shell().scroll(SCROLL_STEP as isize);
                        return Ok(());
                    }
                    MouseEventKind::ScrollDown => {
                        self.tabs[self.active].active_shell().scroll(-(SCROLL_STEP as isize));
                        return Ok(());
                    }
                    _ => {}
                }
            }
            let (col, row) = (mouse.column - self.sidebar_width, mouse.row);
            if let Some(bytes) = encode_mouse(&mouse, col, row, &modes) {
                self.write_active(&bytes)?;
            }
        }
        Ok(())
    }

    /// Open a new shell right after the active tab, starting in the active
    /// tab's working directory (falling back to the base path when there is no
    /// active tab or its cwd can't be read), with no command to replay.
    fn open_tab(&mut self) -> Result<(), Box<dyn Error>> {
        let cwd = self.tabs.get(self.active).and_then(Tab::cwd).unwrap_or_else(|| self.base.clone());
        let color = tab_color(self.created);
        self.created += 1;
        let tab = Tab::spawn(self.pty_rows, self.pty_cols, &cwd, color, None)?;
        let at = (self.active + 1).min(self.tabs.len());
        self.tabs.insert(at, tab);
        self.active = at;
        Ok(())
    }

    /// Spawn a subshell in the active tab, in the active shell's directory
    /// (falling back to the base path), and focus it. The subshell shares the
    /// tab's color and cannot be reordered independently of the tab.
    fn open_subshell(&mut self) -> Result<(), Box<dyn Error>> {
        let (rows, cols, base) = (self.pty_rows, self.pty_cols, self.base.clone());
        let tab = &mut self.tabs[self.active];
        let cwd = tab.cwd().unwrap_or(base);
        tab.shells.push(Shell::spawn(rows, cols, &cwd, None)?);
        tab.active = tab.shells.len() - 1;
        Ok(())
    }

    /// Append a restored shell at the end (restore preserves saved order).
    fn open_tab_at(&mut self, cwd: &Path, cmd: Option<String>) -> Result<(), Box<dyn Error>> {
        let color = tab_color(self.created);
        self.created += 1;
        self.tabs.push(Tab::spawn(self.pty_rows, self.pty_cols, cwd, color, cmd)?);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    /// Persist every open tab's folder and running command so the session
    /// reopens as-is. Writes only on change; reached only with tabs open.
    fn persist_session(&mut self) {
        let states: Vec<TabState> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabState {
                // Persist the parent shell; subshells are session-local.
                cwd: t.shells[0].cwd().unwrap_or_else(|| self.base.clone()),
                cmd: t.shells[0].foreground_cmd(),
                active: i == self.active,
            })
            .collect();
        if states != self.saved_session {
            save_session(&states);
            self.saved_session = states;
        }
    }

    /// Close the active shell of the active tab; when it was the tab's last
    /// shell, the tab itself closes on the next reap.
    fn close_active(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let _ = tab.active_shell_mut().child.kill();
        }
    }

    /// Drop shells whose child has exited, then tabs left with no shell,
    /// keeping every active index pointed at a surviving neighbour.
    fn reap_dead_tabs(&mut self) {
        for tab in &mut self.tabs {
            let alive: Vec<bool> =
                tab.shells.iter_mut().map(|s| matches!(s.child.try_wait(), Ok(None))).collect();
            let dead_before = alive[..tab.active.min(alive.len())].iter().filter(|a| !**a).count();
            tab.active = tab.active.saturating_sub(dead_before);
            let mut keep = alive.iter();
            tab.shells.retain(|_| *keep.next().unwrap());
            tab.active = tab.active.min(tab.shells.len().saturating_sub(1));
        }
        self.tabs.retain(|t| !t.shells.is_empty());
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
    }

    fn write_active(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
        let shell = self.tabs[self.active].active_shell_mut();
        shell.scroll(0); // any input snaps the view back to live output
        shell.writer.write_all(bytes)?;
        shell.writer.flush()?;
        Ok(())
    }

    /// Keep every PTY sized to the terminal pane; resize on change.
    /// Degenerate areas (e.g. a 0×0 host PTY) are ignored — vt100 cannot
    /// represent screens that small.
    fn fit_ptys(&mut self, area: Rect) {
        self.term_width = area.width;
        if area.height < 3 || area.width <= self.sidebar_width + 1 {
            return;
        }
        let rows = area.height - 1; // one line reserved for the status bar
        let cols = area.width - self.sidebar_width;
        if (rows, cols) != (self.pty_rows, self.pty_cols) {
            (self.pty_rows, self.pty_cols) = (rows, cols);
            for tab in &mut self.tabs {
                tab.resize(rows, cols);
            }
        }
    }
}

// ── ui ───────────────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [sidebar, pane] =
        Layout::horizontal([Constraint::Length(app.sidebar_width), Constraint::Min(1)]).areas(body);

    let items =
        app.tabs.iter().enumerate().map(|(i, tab)| tab_item(i, tab, i == app.active, app.sidebar_width));
    let tab_list = List::new(items)
        .block(Block::new().borders(Borders::RIGHT).title(Line::from(" ricon ").bold().centered()));
    // `list_offset` is the authoritative scroll position: the wheel moves it and
    // a changed active tab is revealed into it (see `reveal_active`), so the
    // render only honours it — no `with_selected`, which would yank the view
    // back to the active tab and fight free scrolling. Clamp first as the tab
    // count or sidebar height may have shrunk since the last scroll.
    app.sidebar_rows = sidebar.height;
    app.list_offset = app.list_offset.min(app.max_offset());
    let mut state = ListState::default().with_offset(app.list_offset);
    frame.render_stateful_widget(tab_list, sidebar, &mut state);
    app.list_offset = state.offset();

    if let Some(tab) = app.tabs.get(app.active) {
        let shell = tab.active_shell();
        let parser = shell.parser.lock().unwrap_or_else(PoisonError::into_inner);
        frame.render_widget(PseudoTerminal::new(parser.screen()), pane);
        frame.render_widget(
            status_bar(app.active, app.tabs.len(), shell, tab.color, footer.width, app.tabs_overflow()),
            footer,
        );
    }
}

/// Footer: an up-down arrow when the tab list overflows, then the active
/// terminal number / tab count, its location and git branch on the left, the
/// app version pinned to the right corner.
fn status_bar(index: usize, count: usize, shell: &Shell, color: Color, width: u16, tabs_overflow: bool) -> Line<'static> {
    let cwd = shell.cwd();
    let path = cwd.as_deref().map_or_else(|| "?".into(), |p| abbreviate_home(&p.display().to_string()));
    let branch = cwd.as_deref().and_then(git_branch).map_or_else(String::new, |b| format!("  ⎇ {b}"));
    let agent = shell.agent.as_ref().map_or_else(String::new, |a| format!("  ✳ {}", a.model));
    let right = format!("v{} ", env!("CARGO_PKG_VERSION"));
    // Scroll indicator sits before the active tab index when not all tabs fit.
    let scroll = if tabs_overflow { "↕ " } else { "" };
    // Left segment, truncated so the right-corner version always fits.
    let mut left = format!(" {scroll}{}/{count} ▸ {path}{branch}{agent}", index + 1);
    let room = (width as usize).saturating_sub(right.chars().count());
    if left.chars().count() > room {
        left = left.chars().take(room).collect();
    }
    let pad = (width as usize).saturating_sub(left.chars().count() + right.chars().count());
    Line::from(format!("{left}{}{right}", " ".repeat(pad)))
        .style(Style::new().bg(color).fg(Color::White).bold())
}

/// First known agent process descending from `shell_pid`, with its spec.
/// One `/proc` scan for all agents (reads each comm once, walks ppid chains
/// only for the rare comm matches). The `/proc/<pid>/children` file is
/// unreliable (often empty), so a downward tree walk would miss agents
/// launched behind a wrapper (e.g. `ollama launch opencode`).
fn detect_agent(shell_pid: u32) -> Option<(&'static AgentSpec, u32)> {
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
        let comm = proc_comm(pid)?;
        let spec = AGENTS.iter().find(|s| s.comm == comm)?;
        descends_from(pid, shell_pid).then_some((spec, pid))
    })
}

/// Whether `pid` has `target` as an ancestor (walking ppid chains, capped).
fn descends_from(pid: u32, target: u32) -> bool {
    let mut cur = pid;
    for _ in 0..64 {
        match proc_ppid(cur) {
            Some(ppid) if ppid == target => return true,
            Some(ppid) if ppid > 1 => cur = ppid,
            _ => return false,
        }
    }
    false
}

fn proc_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 2..].split_whitespace().nth(1)?.parse().ok()
}

fn proc_comm(pid: u32) -> Option<String> {
    Some(std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?.trim().to_string())
}

/// Foreground process-group pid of the shell's PTY (tpgid from
/// /proc/<shell>/stat), only when a process other than the shell holds it.
fn foreground_pid(shell: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{shell}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 2..];
    let tpgid: i32 = after_comm.split_whitespace().nth(5)?.parse().ok()?;
    (tpgid > 0 && tpgid as u32 != shell).then_some(tpgid as u32)
}

/// Resolve a model name from a single source, reading agent process `pid`.
fn resolve_source(src: &Source, pid: u32) -> Option<String> {
    match src {
        Source::Settings(rel, key) => settings_value(rel, key),
        Source::EnvJson(var, key) => env_var(pid, var).and_then(|json| json_string(&json, key)),
        Source::EnvPlain(var) => env_var(pid, var),
        Source::LogTail(dir, key) => log_tail_value(dir, key),
        Source::OpencodeSelected => opencode_selected(pid),
    }
}

/// Last `key=value` token in the most recently modified `*.log` under the
/// `$HOME`-relative `dir`; only the file's tail is read to bound the cost.
fn log_tail_value(dir: &str, key: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let newest = std::fs::read_dir(format!("{home}/{dir}"))
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)?
        .1;
    let buf = read_tail(&newest, 64 * 1024)?;
    let text = String::from_utf8_lossy(&buf);
    let needle = format!("{key}=");
    let rest = &text[text.rfind(&needle)? + needle.len()..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let val = rest[..end].trim_matches('"');
    (!val.is_empty()).then(|| val.to_string())
}

/// opencode's currently selected model for the agent process's project dir.
/// opencode is event-sourced into a SQLite DB; the live selection is the model
/// of the most recently updated `session` row for that directory. Reading it
/// via SQLite (vs. scanning bytes) is essential — WAL frame ordering makes raw
/// scans return stale models. Opened read-only so opencode is never disturbed.
fn opencode_selected(pid: u32) -> Option<String> {
    use rusqlite::{Connection, OpenFlags};
    let home = std::env::var("HOME").ok()?;
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let dir = cwd.to_str()?;
    let db = format!("{home}/.local/share/opencode/opencode.db");
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(100));
    let model: String = conn
        .query_row(
            "SELECT model FROM session \
             WHERE directory = ?1 AND model IS NOT NULL \
             ORDER BY time_updated DESC LIMIT 1",
            [dir],
            |row| row.get(0),
        )
        .ok()?;
    json_string(&model, "id")
}

/// Read up to the last `max` bytes of a file (for cheaply tailing large logs).
fn read_tail(path: &Path, max: u64) -> Option<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Value of env var `var` in process `pid` (read from /proc/<pid>/environ).
fn env_var(pid: u32, var: &str) -> Option<String> {
    let prefix = format!("{var}=");
    std::fs::read(format!("/proc/{pid}/environ"))
        .ok()?
        .split(|b| *b == 0)
        .filter_map(|kv| std::str::from_utf8(kv).ok())
        .find_map(|kv| kv.strip_prefix(&prefix).map(str::to_string))
        .filter(|m| !m.is_empty())
}

/// String value for `key` in the `$HOME`-relative JSON settings file at `rel`.
fn settings_value(rel: &str, key: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let text = std::fs::read_to_string(format!("{home}/{rel}")).ok()?;
    json_string(&text, key)
}

/// First string value for `"key"` in a JSON blob (naive, brace-agnostic — good
/// enough for flat settings; `"key"` matches only the exact key, not `"keys"`).
fn json_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let rest = &text[text.find(&needle)? + needle.len()..];
    let start = rest.find('"')? + 1;
    let end = start + rest[start..].find('"')?;
    Some(rest[start..end].to_string()).filter(|v| !v.is_empty())
}

/// Active git branch for `dir`, if it lies inside a git repository.
/// Reads .git/HEAD directly (no subprocess); follows `gitdir:` worktree files;
/// detached HEAD shows the short commit hash.
fn git_branch(dir: &Path) -> Option<String> {
    dir.ancestors().find_map(|d| {
        let dotgit = d.join(".git");
        let gitdir = if dotgit.is_dir() {
            dotgit
        } else {
            let link = std::fs::read_to_string(&dotgit).ok()?;
            d.join(link.strip_prefix("gitdir:")?.trim())
        };
        let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
        let head = head.trim();
        Some(match head.strip_prefix("ref: refs/heads/") {
            Some(branch) => branch.to_string(),
            None => format!("@{}", head.get(..7).unwrap_or("?")),
        })
    })
}

/// Tab entry. The parent shell takes four rows: folder name (with unseen-output
/// `*`), full location path, running process with the activity spinner appended
/// while output is streaming (+1 s after it settles), and an empty fourth row.
/// Each subshell then adds two rows — its path and running process — shown bold
/// while it is the tab's active shell.
fn tab_item(index: usize, tab: &Tab, is_active: bool, width: u16) -> ListItem<'static> {
    let style = Style::new().bg(tab.color).fg(Color::White);
    let spin_style = Style::new().bg(tab.color).fg(SPINNER_COLOR).bold();
    let parent = &tab.shells[0];
    let cwd = parent.cwd();
    let marker = if is_active { "▶" } else { " " };
    // Any shell producing output while off screen flags the whole tab.
    let star = if tab.shells.iter().any(|s| s.unseen_output) { " *" } else { "" };
    let folder = cwd.as_deref().map_or_else(|| "?".into(), folder_name);
    let name = format!("{marker}{} {}{star}", index + 1, truncate_tail(&folder, width, 4));
    let full = cwd.as_deref().map_or_else(|| "?".into(), |p| abbreviate_home(&p.display().to_string()));
    let path = format!("   {}", truncate_tail(&full, width, 4));
    let top = if is_active { style.bold() } else { style };
    // Rows 1–4 for the parent: name, path, process (+ spinner), empty row.
    let mut lines = vec![
        Line::styled(name, top),
        Line::styled(path, style),
        Line::from(process_row(parent, style, spin_style)),
        Line::styled(String::new(), style),
    ];
    // Two rows per subshell — path and process — bold while it is active.
    for (si, sub) in tab.shells.iter().enumerate().skip(1) {
        let s = if tab.active == si { style.bold() } else { style };
        let scwd = sub.cwd();
        let sfull = scwd.as_deref().map_or_else(|| "?".into(), |p| abbreviate_home(&p.display().to_string()));
        lines.push(Line::styled(format!("   {}", truncate_tail(&sfull, width, 4)), s));
        lines.push(Line::from(process_row(sub, s, spin_style)));
    }
    ListItem::new(lines).style(style)
}

/// A `└ process` row for one shell, with the braille activity spinner appended
/// while its output is streaming (+1 s after it settles).
fn process_row(shell: &Shell, style: Style, spin_style: Style) -> Vec<Span<'static>> {
    // Braille spinner at 0.5 rps: one rotation per 2 s (10 frames × 200 ms).
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let mut row = vec![Span::styled(format!("   └ {}", shell.running_process()), style)];
    if shell.animating {
        let frame = (shell.spawned.elapsed().as_millis() / 200) as usize % FRAMES.len();
        row.push(Span::styled(format!("  {}", FRAMES[frame]), spin_style));
    }
    row
}

/// Final path component (the current folder); root-style paths show as-is.
fn folder_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Tail-truncate `s` to the sidebar width, leaving `pad` columns for the
/// border, marker and indent; an elided head is marked with `…`.
fn truncate_tail(s: &str, width: u16, pad: u16) -> String {
    let max = width.saturating_sub(pad) as usize;
    match s.char_indices().nth_back(max.saturating_sub(1)) {
        Some((cut, _)) if cut > 0 => format!("…{}", &s[cut..]),
        _ => s.to_string(),
    }
}

// ── pure helpers ─────────────────────────────────────────────────────────────

/// Base path: derived from the first CLI parameter when given (~-expanded,
/// canonicalized), otherwise the directory the app was started from.
fn base_path(arg: Option<String>) -> Result<PathBuf, Box<dyn Error>> {
    match arg {
        Some(raw) => std::fs::canonicalize(expand_home(&raw))
            .map_err(|e| format!("invalid base path {raw:?}: {e}").into()),
        None => Ok(std::env::current_dir()?),
    }
}

fn expand_home(path: &str) -> PathBuf {
    match (std::env::var("HOME"), path.strip_prefix("~")) {
        (Ok(home), Some(rest)) if rest.is_empty() || rest.starts_with('/') => PathBuf::from(home + rest),
        _ => PathBuf::from(path),
    }
}

fn abbreviate_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path.starts_with(&home) => path.replacen(&home, "~", 1),
        _ => path.to_string(),
    }
}

/// Session file under XDG state home: one tab per line as `[>]folder\tcommand`
/// — the command is omitted when only the shell ran, and a leading `>` marks
/// the tab that was active (paths are absolute, so the marker is unambiguous).
fn session_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("ricon").join("session"))
}

/// Tabs open at last save, in order (folder + replayable command); empty if none.
fn load_session() -> Vec<TabState> {
    session_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(|l| {
                    let (active, rest) = match l.strip_prefix('>') {
                        Some(r) => (true, r),
                        None => (false, l),
                    };
                    let (cwd, cmd) = match rest.split_once('\t') {
                        Some((cwd, cmd)) => (cwd, Some(cmd.to_string())),
                        None => (rest, None),
                    };
                    TabState { cwd: cwd.into(), cmd, active }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_session(states: &[TabState]) {
    let Some(path) = session_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: Vec<String> = states
        .iter()
        .map(|s| {
            let mark = if s.active { ">" } else { "" };
            match &s.cmd {
                Some(cmd) => format!("{mark}{}\t{cmd}", s.cwd.display()),
                None => format!("{mark}{}", s.cwd.display()),
            }
        })
        .collect();
    let _ = std::fs::write(path, body.join("\n"));
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
}

/// Distinct, cool-biased tab background for the n-th created tab: a curated
/// ring of modern jewel hues (azure → violet → teal → indigo, no warm
/// reds/oranges/yellows) sharing one saturation/lightness so the palette reads
/// as a cohesive set; consecutive hues are spread far apart for legibility, and
/// each full cycle nudges the shade lighter so tabs past eight stay distinct.
fn tab_color(n: usize) -> Color {
    const HUES: [f32; 8] = [210.0, 280.0, 165.0, 250.0, 190.0, 310.0, 230.0, 150.0];
    let lightness = 0.30 + ((n / HUES.len()) % 3) as f32 * 0.07;
    hsl_rgb(HUES[n % HUES.len()], 0.46, lightness)
}

/// The activity spinner's single color, per the kata: white.
const SPINNER_COLOR: Color = Color::Rgb(255, 255, 255);

fn hsl_rgb(h: f32, s: f32, l: f32) -> Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h as u16) / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let byte = |v: f32| ((v + m) * 255.0).round() as u8;
    Color::Rgb(byte(r), byte(g), byte(b))
}

/// Translate a key event into the byte sequence a terminal would send,
/// honoring DECCKM (application cursor keys) and xterm modifier encoding.
fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8; // xterm modifier code

    let cursor = |c: char| match (m, app_cursor) {
        (1, false) => format!("\x1b[{c}").into_bytes(),
        (1, true) => format!("\x1bO{c}").into_bytes(),
        _ => format!("\x1b[1;{m}{c}").into_bytes(),
    };
    let tilde = |n: u8| match m {
        1 => format!("\x1b[{n}~").into_bytes(),
        _ => format!("\x1b[{n};{m}~").into_bytes(),
    };

    let mut bytes = match key.code {
        KeyCode::Char(c) if ctrl && c.is_ascii() => vec![(c.to_ascii_uppercase() as u8) & 0x1f],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab if shift => b"\x1b[Z".to_vec(),
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor('A'),
        KeyCode::Down => cursor('B'),
        KeyCode::Right => cursor('C'),
        KeyCode::Left => cursor('D'),
        KeyCode::Home => cursor('H'),
        KeyCode::End => cursor('F'),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::F(n @ 1..=4) if m == 1 => vec![0x1b, b'O', b'O' + n],
        KeyCode::F(n @ 1..=4) => format!("\x1b[1;{m}{}", (b'O' + n) as char).into_bytes(),
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        _ => return None,
    };
    // Alt on text-producing keys is the classic ESC prefix; on the special
    // keys above it is already carried by the modifier parameter.
    let text_key = matches!(
        key.code,
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace | KeyCode::Tab | KeyCode::Esc
    );
    if alt && text_key {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// Translate a mouse event into the inner app's requested mouse protocol.
/// `col`/`row` are 0-based, relative to the terminal pane.
fn encode_mouse(mouse: &MouseEvent, col: u16, row: u16, modes: &TermModes) -> Option<Vec<u8>> {
    use MouseEventKind as K;
    use MouseProtocolMode as M;

    let button = |b: MouseButton| match b {
        MouseButton::Left => 0u8,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    // (base button code, is-press) — gated by what the app subscribed to.
    let (mut cb, press) = match (mouse.kind, modes.mouse_mode) {
        (_, M::None) => return None,
        (K::Down(b), _) => (button(b), true),
        (K::Up(b), M::PressRelease | M::ButtonMotion | M::AnyMotion) => (button(b), false),
        (K::Drag(b), M::ButtonMotion | M::AnyMotion) => (button(b) + 32, true),
        (K::Moved, M::AnyMotion) => (3 + 32, true),
        (K::ScrollUp, _) => (64, true),
        (K::ScrollDown, _) => (65, true),
        _ => return None,
    };
    cb += 4 * mouse.modifiers.contains(KeyModifiers::SHIFT) as u8
        + 8 * mouse.modifiers.contains(KeyModifiers::ALT) as u8
        + 16 * mouse.modifiers.contains(KeyModifiers::CONTROL) as u8;

    Some(match modes.mouse_encoding {
        MouseProtocolEncoding::Sgr => {
            let suffix = if press { 'M' } else { 'm' };
            format!("\x1b[<{cb};{};{}{suffix}", col + 1, row + 1).into_bytes()
        }
        // Legacy (and utf8) encoding: release loses the button identity.
        _ => {
            let cb = if press { cb } else { 3 };
            let clamp = |v: u16| 32 + (v + 1).min(222) as u8;
            vec![0x1b, b'[', b'M', 32 + cb, clamp(col), clamp(row)]
        }
    })
}
