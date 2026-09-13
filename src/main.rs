//! ricon — a console with vertical tabs (ratatui + portable-pty + vt100).

use std::{
    cell::Cell,
    error::Error,
    hash::{DefaultHasher, Hash, Hasher},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem},
};
use tui_term::{
    vt100::{self, MouseProtocolEncoding, MouseProtocolMode},
    widget::PseudoTerminal,
};

mod transcript;
use transcript::{Meta, Pump, Transcript};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SIDEBAR_WIDTH: u16 = 26;
const MIN_SIDEBAR_WIDTH: u16 = 8;
const MIN_PANE_WIDTH: u16 = 10;
/// One frame: the app draws at most this often and spends the rest of the
/// budget waiting for input. Every event already queued is then handled before
/// the next draw — a mouse-motion or paste burst must cost one frame, never one
/// full render per event.
const POLL_INTERVAL: Duration = Duration::from_millis(30);
/// Ceiling on one drain pass, so an unbroken flood of events (an app in
/// any-motion mouse mode under a moving cursor) can never starve the redraw.
const DRAIN_BUDGET: Duration = Duration::from_millis(8);
/// How long the "✓ copied" footer hint lingers after a copy to the clipboard.
const COPY_HINT: Duration = Duration::from_millis(1200);
/// Clicks landing on the same cell within this window chain into a double
/// (word) then triple (line) click.
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// Labels of the footer's clickable button, pinned just left of the version
/// corner: it shows and toggles copy mode (kata ui.md) — on by default, so a
/// plain drag selects text even over an app that grabbed the mouse; off hands
/// the whole mouse to that app. Both are `BUTTON_COLS` wide so render and
/// hit-test always agree.
const COPY_ON_BUTTON: &str = " ⧉ copy ✓ ";
const COPY_OFF_BUTTON: &str = " ⧉ copy ✗ ";
const BUTTON_COLS: u16 = 10;
/// The one tab color (kata app.md): a soft pastel for the active tab, with a
/// dark foreground so its text stays legible on it. Inactive tabs paint no
/// background at all and take the terminal's own colors. The footer wears the
/// same pastel, so the active tab and its status read as one.
const ACTIVE_BG: Color = Color::Rgb(186, 200, 232);
const ACTIVE_FG: Color = Color::Rgb(24, 28, 44);
/// The activity bar on the active tab's last row (kata app.md): a quiet track
/// the whole row long, and a lit segment `BAR_LEN` cells wide that sweeps along
/// it — one cell per `BAR_STEP` — while the tab's output is streaming.
const BAR_TRACK: Color = Color::Rgb(120, 136, 176);
const BAR_LIT: Color = Color::Rgb(36, 64, 150);
const BAR_LEN: usize = 6;
const BAR_STEP: Duration = Duration::from_millis(40);
/// Label of a tab's `auto` button (kata ai.md), sitting on the tab's first
/// sidebar line after its text and indicators, glued to the right edge of the
/// panel. Its background carries the state — green when the tab's continue
/// feature is on, gray when off — and it is `AUTO_COLS` wide so render and
/// hit-test agree.
const AUTO_BUTTON: &str = " ⟳ auto ";
const AUTO_COLS: u16 = 8;
/// The two states of that button (kata ai.md). Each carries its own foreground
/// so the label stays legible on either background — the green is deep enough
/// to carry white text (and the yellow nudge mark) at readable contrast. A
/// third foreground marks a nudge this tab's agent has not answered yet — the
/// auto feature acts on every tab, including the ones off screen, so the
/// sidebar has to show where it spoke. It clears on that agent's next output,
/// like the footer's own mark.
const AUTO_ON_BG: Color = Color::Rgb(30, 120, 60);
const AUTO_ON_FG: Color = Color::White;
const AUTO_NUDGED_FG: Color = Color::Yellow;
const AUTO_OFF_BG: Color = Color::DarkGray;
const AUTO_OFF_FG: Color = Color::White;
/// Silence after which the auto feature nudges a detected AI agent: no output
/// change for ten minutes means it is waiting rather than working (kata
/// ai.md) — long enough that a slow tool call or a long generation is never
/// mistaken for an agent asking a question.
const IDLE_NUDGE: Duration = Duration::from_secs(10 * 60);
/// How long a nudge mark stays up before the agent's next answer may clear it.
/// The agent echoes the typed sentence within milliseconds, so a mark that
/// cleared on the first content change after the nudge would never be seen —
/// and the auto feature must never type on the user's behalf invisibly.
const NUDGE_MARK: Duration = Duration::from_secs(30);
/// Gap left between typing a sentence into an AI client and confirming it
/// (kata ai.md: the text is *confirmed*, not just typed). TUI agents read
/// their tty in chunks and treat one chunk of many bytes as pasted text — a
/// trailing `\r` inside it is taken as a literal newline in the composer, so
/// the message is never sent. Delivered as its own read, after the client has
/// settled, the `\r` is an Enter keypress, which is what actually submits.
/// Half a second: an order of magnitude over the ~50ms these clients coalesce
/// input within, and free — it is paid once per ten-minute silence, on a
/// writer thread, and no one is waiting on it.
const SUBMIT_SETTLE: Duration = Duration::from_millis(500);
/// What the auto feature types into an idle agent, in order (kata ai.md):
/// first the client's compact command, so the conversation is summarized
/// before it goes on; then — once the compaction has visibly finished — the
/// continue text: the project's own `.ai/auto.md` when it has one, else the
/// plain word.
const COMPACT_COMMAND: &str = "/compact";
const DEFAULT_CONTINUE: &str = "continue";
const AUTO_FILE: &str = ".ai/auto.md";
/// A compaction is done once the screen changed after the command was typed
/// and has then stood still this long. The client keeps redrawing a spinner
/// while it summarizes, so a still screen after any change is the summary
/// landed — and this is far longer than any redraw gap while it works.
const COMPACT_SETTLE: Duration = Duration::from_secs(15);
/// A compaction that never shows a change (a client without the command, or
/// one that swallowed it) is not waited on forever: the continue text follows
/// after this long regardless.
const COMPACT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Proactive compaction (kata ai.md): with the context this full, an agent
/// that has been idle for `CONTEXT_SETTLE` is compacted without waiting out
/// the full silence — and never twice within `CONTEXT_COMPACT_EVERY`, since
/// the usage the probe reads is the *last answer's* and only drops once the
/// agent has answered again after the compaction.
const CONTEXT_COMPACT_AT: f64 = 0.70;
const CONTEXT_SETTLE: Duration = Duration::from_secs(60);
const CONTEXT_COMPACT_EVERY: Duration = Duration::from_secs(10 * 60);
/// The footer paints the usage red from here on.
const CONTEXT_WARN_AT: f64 = 0.80;
/// The footer shows the countdown to the next nudge once an agent has been
/// idle this long — sooner, and the screen-hash fallback would flicker it up
/// between an agent's tool calls.
const COUNTDOWN_FROM: Duration = Duration::from_secs(30);
/// An agent off screen that has been idle this long gets a desktop ping
/// (kata ai.md): a beat of debounce when the client reports its own status,
/// a full minute when only the screen hash says so.
const PING_AFTER_STATUS: Duration = Duration::from_secs(3);
const PING_AFTER_SCREEN: Duration = Duration::from_secs(60);
/// Environment variables that describe the terminal ricon was *started from*.
/// They are inherited with the rest of the environment but say nothing true
/// about the emulator a shell in here actually talks to, so every one is
/// cleared before spawning it.
///
/// This is not cosmetic. `VTE_VERSION` makes `/etc/profile.d/vte-2.91.sh` turn
/// GNOME's shell integration on inside ricon, after which bash writes OSC 7 and
/// OSC 133 semantic-prompt sequences at every prompt — capabilities this vt100
/// emulator does not implement. `COLUMNS`/`LINES`, when the host shell exported
/// them, override the PTY's real size and wrap output at the wrong column. Both
/// only ever showed up under bash: dash sources neither the profile snippet nor
/// readline, which is why `sh` looked fine while `bash` did not.
const HOST_TERMINAL_VARS: &[&str] =
    &["VTE_VERSION", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "COLUMNS", "LINES"];
/// How often a transcribed shell's console is walked onto disk while it is
/// quiet — the reader thread covers everything that arrives, this covers the
/// stretch after the last byte (kata ai.md).
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// Host-side scrollback retained per tab, and lines moved per wheel notch.
const SCROLLBACK_LINES: usize = 5000;
const SCROLL_STEP: usize = 3;
/// Rule: the activity animation lasts one more second after output settles.
const SETTLE: Duration = Duration::from_secs(1);
/// Output within this window after a PTY resize is the shell repainting its
/// prompt on SIGWINCH, not real activity — it must trigger neither the `*`
/// marker nor the spinner animation.
const RESIZE_GRACE: Duration = Duration::from_secs(1);
/// /proc- and disk-derived caches (cwd, process, command line, agent, git
/// branch) refresh at 2 Hz — the render loop must never do IO every frame.
const SAMPLE_EVERY: Duration = Duration::from_millis(500);

fn main() -> Result<(), Box<dyn Error>> {
    let mut terminal = ratatui::init();
    // Kitty keyboard protocol (where supported): hosts then report numpad keys
    // with full modifiers, so alt+numpad-digit works like alt+main-row-digit.
    let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        let _ = execute!(
            std::io::stdout(),
            // Alternate keys make the host report the shifted character, so
            // Alt+Shift+f arrives as Alt+'F' (and Alt+Shift+/ as Alt+'?'), not
            // as Alt+'f' with a Shift flag nothing downstream reads.
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
    }
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    // A panic must not leave the host terminal grabbing the mouse and eating
    // pastes — that reads as a frozen terminal long after ricon is gone.
    // ratatui's own hook restores raw mode and the alternate screen; ours runs
    // first and gives back everything ricon turned on itself.
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        release_host_terminal(enhanced);
        ratatui_hook(info);
    }));
    let result = App::new().and_then(|mut app| app.run(&mut terminal));
    release_host_terminal(enhanced);
    ratatui::restore();
    result
}

/// Hand back every host-terminal mode ricon switched on. Idempotent and
/// best-effort: it runs on the normal exit path and from the panic hook alike.
fn release_host_terminal(enhanced: bool) {
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
}

// ── tab ──────────────────────────────────────────────────────────────────────

/// One item in a shell's outbound queue. `Settle` is a deliberate gap between
/// two writes: it keeps the inner app from reading them as a single chunk,
/// which is how a typed sentence and its Enter end up merged into one paste.
enum Out {
    Bytes(Vec<u8>),
    Settle,
}

struct Shell {
    parser: Arc<Mutex<vt100::Parser>>,
    /// Input queue drained by this shell's writer thread. A PTY write blocks
    /// once the inner app stops reading (a big paste into a busy program fills
    /// the tty buffer), so the UI thread must never perform one itself.
    input: mpsc::Sender<Out>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    /// Output activity: bumped by the reader thread, consumed by the UI tick.
    activity: Arc<AtomicU64>,
    seen_activity: u64,
    spawned: Instant,
    last_change: Instant,
    /// Last time the *visible screen* changed, which is what the auto feature
    /// calls activity: the kata's silence is "console content is the same"
    /// (kata ai.md), and an agent waiting for the user keeps emitting bytes
    /// that repaint the very same screen (cursor blink, redraw ticks).
    last_content_change: Instant,
    /// Hash of the screen at that moment — one `u64` instead of a copy of it.
    content: u64,
    /// Last PTY resize; shell repaints right after are not "new output".
    resized: Instant,
    animating: bool,
    /// Output arrived while this tab was not the active one; cleared on focus.
    unseen_output: bool,
    /// Where the auto feature is in nudging this idle stretch — `None` when
    /// it is not — one compact-then-continue per silence, and the mark the
    /// sidebar and footer draw until the agent answers (never sooner than
    /// `NUDGE_MARK`, so it is always seen).
    nudge: Option<Nudge>,
    /// When the auto feature last typed anything here: a nudge starts the idle
    /// clock over, whatever the client's own status says about it.
    last_nudge: Option<Instant>,
    /// When the user last typed into this shell: the idle clock never runs
    /// past it, so typing into the composer defers a nudge.
    last_input: Instant,
    /// When a due nudge last read whether its agent holds the tty.
    tty_checked: Cell<Instant>,
    /// When the context usage last triggered a compaction — see
    /// `CONTEXT_COMPACT_EVERY`.
    context_compacted: Option<Instant>,
    /// The desktop ping for this idle stretch has gone out (or the stretch
    /// was seen on screen); reset once the agent is busy again.
    pinged: bool,
    /// AI coding agent detected in this tab's shell — resolved off-thread by
    /// `AgentProbe` and posted back here (detection walks all of /proc and may
    /// read a database, so it never runs on the render path).
    agent: Option<AgentInfo>,
    /// Cached working directory, refreshed by `tick_proc`.
    cwd: Option<PathBuf>,
    /// Cached foreground process name, refreshed by `tick_proc`.
    process: String,
    /// Cached foreground command line, refreshed by `tick_proc` — what the
    /// session persists and a restart replays.
    fg_cmd: Option<String>,
    /// Last command the user typed and confirmed with Enter at this shell's
    /// prompt — what the sidebar `replay` button and Alt+r type-and-confirm
    /// again (kata app.md). Captured from keystrokes + the shell's echo by
    /// `note_input`, never from /proc, so fast commands and builtins count too.
    last_cmd: Option<String>,
    /// Where the command line being typed begins on screen (row, col — the
    /// prompt's end); `None` between commands. Anchors the echo read that
    /// `note_input` snapshots into `last_cmd` on Enter.
    cmd_anchor: Option<(u16, u16)>,
    /// Restored command to replay once the shell's first prompt is up.
    pending_cmd: Option<String>,
    /// Session transcript (kata ai.md): this shell's console text, appended to
    /// a dated file from the moment an AI client is detected in it. Armed here,
    /// written by the reader thread, closed by `finish_log`.
    log: Arc<Transcript>,
}

/// The two steps of one auto-feature nudge (kata ai.md), each stamped with
/// when it was typed: the compact command, then — once the compaction has
/// settled — the continue text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Nudge {
    Compacting(Instant),
    Continued(Instant),
}

#[derive(Clone, Debug, PartialEq)]
struct AgentInfo {
    /// The client's process name (`claude`, `openclaude`, `opencode`) — what
    /// the transcript file is named after.
    name: &'static str,
    model: String,
    /// The agent process itself. The auto feature types a sentence, so it must
    /// be sure this process is the one reading the tty — see `agent_has_tty`.
    pid: u32,
    /// How full the model's context is (kata ai.md): tokens in use, and the
    /// window they sit in when it is known.
    context: Option<Context>,
    /// What the client itself says it is doing, when it says anything (Claude
    /// Code registers `idle`/`busy` with a timestamp) — exact, where the
    /// screen hash is a guess.
    status: Option<Status>,
}

/// An agent's own account of its state: busy, or idle since a wall-clock
/// moment (so the idle time is exact across probes and restarts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Busy,
    Idle(SystemTime),
}

impl Context {
    /// Fraction of the window in use — `None` without a known window.
    fn fill(self) -> Option<f64> {
        self.max.map(|max| self.used as f64 / max.max(1) as f64)
    }
}

/// Context usage of an agent's session: what the last answer was billed for
/// as input — that is the whole conversation as the model saw it — and the
/// window it has to fit in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Context {
    used: u64,
    max: Option<u64>,
}

/// A detectable AI coding agent: the process name to look for, plus an ordered
/// list of places its current model can be read from (first hit wins; if none
/// resolve, the agent's own name is shown as the label).
struct AgentSpec {
    comm: &'static str,
    sources: &'static [Source],
    usage: Usage,
}

/// Where an agent's context usage can be read from.
enum Usage {
    /// The session transcript Claude Code (or a fork) keeps under a
    /// `$HOME`-relative `projects` dir — every answer carries its usage.
    ClaudeSession(&'static str),
    /// opencode's message store: the last answer's token total.
    Opencode,
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
    /// The model of the newest session transcript Claude Code (or a fork) wrote
    /// for the directory the agent runs in, under a `$HOME`-relative `projects`
    /// dir. This is the model that actually answered last, so it follows an
    /// in-session `/model` switch — settings and env cannot.
    SessionTail(&'static str),
    /// opencode's per-directory selected model, read from its state store and
    /// keyed by the agent process's cwd — this is what the opencode TUI shows.
    OpencodeSelected,
}

const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        comm: "claude",
        // The live session transcript first — `settings.json` usually carries
        // no model at all (the model is picked in-session), and the env var is
        // frozen at launch, so without the transcript the status bar could only
        // fall back to the client's own name instead of a model (kata ai.md).
        sources: &[
            Source::SessionTail(".claude/projects"),
            Source::Settings(".claude/settings.json", "model"),
            Source::EnvPlain("ANTHROPIC_MODEL"),
        ],
        usage: Usage::ClaudeSession(".claude/projects"),
    },
    AgentSpec {
        // A Claude Code fork: same transcript and settings shape, its own
        // config directory, and the model it was launched with in the shared
        // Anthropic env var.
        comm: "openclaude",
        sources: &[
            Source::SessionTail(".openclaude/projects"),
            Source::Settings(".openclaude/settings.json", "model"),
            Source::EnvPlain("ANTHROPIC_MODEL"),
        ],
        usage: Usage::ClaudeSession(".openclaude/projects"),
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
        usage: Usage::Opencode,
    },
];

impl Shell {
    fn spawn(rows: u16, cols: u16, cwd: &Path, pending_cmd: Option<String>) -> Result<Self, Box<dyn Error>> {
        let pair = native_pty_system().openpty(pty_size(rows, cols))?;

        let mut cmd = CommandBuilder::new(default_shell());
        cmd.env("TERM", "xterm-256color");
        // `TERM` above is the only terminal identity ricon vouches for. The
        // host's own identity is inherited with the rest of the environment
        // and describes *its* capabilities, not this vt100 emulator's, so it
        // is dropped — see `HOST_TERMINAL_VARS`.
        for var in HOST_TERMINAL_VARS {
            cmd.env_remove(var);
        }
        cmd.cwd(cwd);
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));
        let activity = Arc::new(AtomicU64::new(0));
        let mut reader = pair.master.try_clone_reader()?;
        let feed = Arc::clone(&parser);
        let pulse = Arc::clone(&activity);
        let log = Arc::new(Transcript::new(SCROLLBACK_LINES));
        let tap = Arc::clone(&log);
        thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        feed.lock().unwrap_or_else(PoisonError::into_inner).process(&buf[..n]);
                        // Commit what these bytes just finished drawing (a no-op
                        // unless this shell is being transcribed).
                        tap.pump(&feed, Pump::Output);
                        pulse.fetch_add(1, Ordering::Relaxed);
                        // Under a firehose of output this thread would otherwise
                        // re-take the parser lock before the render thread ever
                        // gets it — the app looked frozen while a command spewed
                        // text. Yielding after each chunk keeps the lock fair.
                        thread::yield_now();
                    }
                    // A signal interrupting the read is not end-of-output;
                    // treating it as one froze the tab with its shell alive.
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            // The shell is gone: close its transcript with the screen that never
            // reached the scrollback.
            tap.close(&feed);
        });

        // Writer thread: owns the PTY writer so a blocking write (inner app not
        // reading) stalls only this queue, never the UI — and so the pause that
        // splits typing from confirming is waited out here rather than on the
        // render thread, which must never sleep.
        let (input, outbox) = mpsc::channel::<Out>();
        let mut writer = pair.master.take_writer()?;
        thread::spawn(move || {
            while let Ok(out) = outbox.recv() {
                match out {
                    Out::Settle => thread::sleep(SUBMIT_SETTLE),
                    Out::Bytes(bytes) => {
                        if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut shell = Self {
            parser,
            input,
            pid: child.process_id(),
            master: pair.master,
            child,
            activity,
            seen_activity: 0,
            spawned: Instant::now(),
            last_change: Instant::now(),
            last_content_change: Instant::now(),
            content: 0,
            resized: Instant::now(),
            animating: false,
            unseen_output: false,
            nudge: None,
            last_nudge: None,
            // Nobody has typed here yet: the other clocks bound the wait.
            last_input: stale(IDLE_NUDGE),
            tty_checked: Cell::new(stale(SAMPLE_EVERY)),
            context_compacted: None,
            pinged: false,
            agent: None,
            cwd: Some(cwd.to_path_buf()),
            process: String::new(),
            fg_cmd: None,
            last_cmd: None,
            cmd_anchor: None,
            pending_cmd,
            log,
        };
        shell.sample_proc();
        Ok(shell)
    }

    /// Replay a restored command once the shell has produced its first prompt
    /// (signalled by any output, plus a small settle), then forget it.
    fn flush_pending(&mut self) {
        const STARTUP: Duration = Duration::from_millis(250);
        let ready = self.activity.load(Ordering::Relaxed) > 0 && self.spawned.elapsed() > STARTUP;
        if ready && let Some(cmd) = self.pending_cmd.take() {
            self.last_cmd = Some(cmd.clone()); // a restored command is replayable at once
            self.send_line(&cmd);
        }
    }

    /// Queue `bytes` for the PTY. Non-blocking by construction: the writer
    /// thread does the (possibly blocking) write.
    fn send(&self, bytes: &[u8]) {
        let _ = self.input.send(Out::Bytes(bytes.to_vec()));
    }

    /// Type `cmd` and confirm it with Enter. For a shell, which reads its tty a
    /// line at a time, one write is enough; a full-screen client needs
    /// `submit` instead.
    fn send_line(&self, cmd: &str) {
        let mut line = cmd.as_bytes().to_vec();
        line.push(b'\r');
        self.send(&line);
    }

    /// Type `text` into a full-screen client and confirm it the way a person
    /// does: paste, pause, Enter (kata ai.md — the text is confirmed, not just
    /// typed). Three separate writes, because that is the whole point:
    ///
    /// * bracketed paste when the client asked for it, so the sentence is
    ///   unambiguously content — never keys the composer might interpret;
    /// * `SUBMIT_SETTLE` so the client finishes taking the text before the
    ///   confirmation arrives, instead of coalescing both into one paste;
    /// * `\r` alone, which lands as an Enter keypress and sends the message.
    fn submit(&self, text: &str) {
        if self.modes().bracketed_paste {
            self.send(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.send(text.as_bytes());
        }
        let _ = self.input.send(Out::Settle);
        self.send(b"\r");
    }

    /// Refresh the cached /proc-derived facts the UI renders and the session
    /// persists: working directory, foreground process name, and its full
    /// command line — one pass, one foreground lookup. A failed cwd read
    /// keeps the last known directory (e.g. a shell mid-exit).
    fn sample_proc(&mut self) {
        if let Some(cwd) = self.read_cwd() {
            self.cwd = Some(cwd);
        }
        let Some(pid) = self.pid else {
            self.process = "?".into();
            self.fg_cmd = None;
            return;
        };
        let fg = foreground_pid(pid);
        self.process = proc_comm(fg.unwrap_or(pid)).or_else(|| proc_comm(pid)).unwrap_or_else(|| "?".into());
        self.fg_cmd = fg.and_then(proc_cmdline);
    }

    /// A replay is offered only while a shell owns the tty (kata app.md §93:
    /// bash or any other shell, never other programs) and a command has been
    /// captured to re-run. Gates the sidebar button, its hit-test and Alt+r.
    fn replayable(&self) -> bool {
        self.last_cmd.is_some() && is_shell(&self.process)
    }

    /// Replay the last command observed in this shell: type it and confirm
    /// with Enter (kata: the red `replay` button / Alt+r). A no-op unless a
    /// shell is the current foreground process.
    fn replay(&mut self) {
        if self.replayable()
            && let Some(cmd) = self.last_cmd.clone()
        {
            self.scroll(0);
            self.send_line(&cmd);
        }
    }

    /// Track input the user sends to the shell so `replay` re-runs exactly the
    /// last command typed and confirmed with Enter (kata app.md §92). ricon
    /// sits between the keyboard and the PTY, so the command is read straight
    /// off the shell's own echo — correct for fast commands, builtins,
    /// pipelines, history recall and tab-completion alike, unlike sampling
    /// /proc. Called on the live view, before the bytes reach the PTY.
    fn note_input(&mut self, bytes: &[u8]) {
        if !self.shell_fg() {
            self.cmd_anchor = None; // a program owns the tty: keys aren't a command line
        } else if bytes.contains(&b'\r') || bytes.contains(&b'\n') {
            // Enter confirms the line: snapshot the echoed text prompt→cursor.
            let end = self.cursor_position();
            let anchor = self.cmd_anchor.take().unwrap_or(end);
            let line = self.screen_between(anchor, end);
            let line = line.trim();
            if !line.is_empty() {
                self.last_cmd = Some(line.to_string());
            }
        } else if bytes.iter().any(|&b| b == 0x03 || b == 0x07) {
            self.cmd_anchor = None; // Ctrl+C / Ctrl+G abandon the line → re-anchor next key
        } else if self.cmd_anchor.is_none() {
            self.cmd_anchor = Some(self.cursor_position()); // first key: anchor at the prompt end
        }
    }

    /// Visible grid row `row` as one char per column (a blank cell reads as a
    /// space), so a column index maps straight to a character — what
    /// double-click word expansion walks.
    fn row_chars(&self, row: u16, cols: u16) -> Vec<char> {
        let parser = self.parser.lock().unwrap_or_else(PoisonError::into_inner);
        let screen = parser.screen();
        (0..cols)
            .map(|c| screen.cell(row, c).and_then(|cell| cell.contents().chars().next()).unwrap_or(' '))
            .collect()
    }

    /// First and last visible row of the logical line through `row`: a line the
    /// terminal soft-wrapped is one line to the user, so a triple-click takes
    /// all of it (and `contents_between` rejoins the pieces).
    fn logical_line(&self, row: u16, rows: u16) -> (u16, u16) {
        let parser = self.parser.lock().unwrap_or_else(PoisonError::into_inner);
        let screen = parser.screen();
        let mut start = row;
        while start > 0 && screen.row_wrapped(start - 1) {
            start -= 1;
        }
        let mut end = row;
        while end + 1 < rows && screen.row_wrapped(end) {
            end += 1;
        }
        (start, end)
    }

    /// Screen cursor as (row, col) in the visible grid.
    fn cursor_position(&self) -> (u16, u16) {
        self.parser.lock().unwrap_or_else(PoisonError::into_inner).screen().cursor_position()
    }

    /// Echoed screen text between two visible-grid positions (wrapping-aware).
    fn screen_between(&self, a: (u16, u16), b: (u16, u16)) -> String {
        self.parser
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .screen()
            .contents_between(a.0, a.1, b.0, b.1)
    }

    /// Is the tty foreground a shell at its prompt? True when the shell holds
    /// its own process group (idle prompt) or the foreground child is itself a
    /// shell. Read live — per keystroke, human-rate — so it never lags a
    /// just-finished command the way the 2 Hz `process` cache would.
    fn shell_fg(&self) -> bool {
        let Some(pid) = self.pid else { return false };
        match foreground_pid(pid) {
            None => true,
            Some(fg) => proc_comm(fg).is_some_and(|c| is_shell(&c)),
        }
    }

    /// Advance the activity animation: the phase moves whenever output
    /// arrived since the last tick; it stops shortly after output settles.
    /// Output arriving while inactive sets the unseen marker; focus clears it.
    fn tick_activity(&mut self, is_active: bool) {
        let now = self.activity.load(Ordering::Relaxed);
        if now != self.seen_activity {
            self.seen_activity = now;
            if self.resized.elapsed() > RESIZE_GRACE {
                self.last_change = Instant::now();
                if !is_active {
                    self.unseen_output = true;
                }
                // The auto feature's silence is content, not bytes (kata
                // ai.md), so the screen itself has to be compared — but only
                // for the shells a nudge could ever fire in, which are the ones
                // holding an agent. Everywhere else the byte clock stands in,
                // and it is the conservative one: it resets more often.
                if self.agent.is_none() || self.screen_changed() {
                    self.last_content_change = self.last_change;
                }
            }
        }
        if is_active {
            self.unseen_output = false;
        }
        self.animating = self.last_change.elapsed() < SETTLE;
    }

    /// Has the visible screen changed since this was last asked? Hashing keeps
    /// one `u64` per shell rather than a copy of the screen, and it runs only
    /// when fresh bytes have already arrived — never on an idle frame.
    fn screen_changed(&mut self) -> bool {
        let mut hasher = DefaultHasher::new();
        self.parser.lock().unwrap_or_else(PoisonError::into_inner).screen().contents().hash(&mut hasher);
        let hash = hasher.finish();
        std::mem::replace(&mut self.content, hash) != hash
    }

    /// Auto feature (kata ai.md): a supported AI client is running in this
    /// shell and the console has shown the same content for `IDLE_NUDGE` — it
    /// is waiting on the user. Two steps, each typed and confirmed: the compact
    /// command first, then — once the compaction has settled — the continue
    /// text. Each step starts a new silence, so nothing repeats per frame; a
    /// second nudge takes another full `IDLE_NUDGE` of unchanged screen. Both
    /// steps are gated on the agent holding the tty: anything else in the
    /// foreground would swallow (or run) the text.
    fn nudge_if_idle(&mut self) {
        match self.nudge {
            // The tty read is throttled: a due nudge whose agent does not hold
            // the tty (suspended, `less` in front) must not cost /proc reads
            // every frame.
            None if self.nudge_due() && self.tty_check_due() && self.agent_has_tty() => {
                let now = Instant::now();
                self.last_content_change = now;
                self.last_nudge = Some(now);
                if self.context_full() {
                    self.context_compacted = Some(now);
                }
                self.nudge = Some(Nudge::Compacting(now));
                self.submit(COMPACT_COMMAND);
            }
            Some(Nudge::Compacting(at)) if self.compaction_done(at) => {
                // The agent went away mid-nudge, or the user came back and has
                // been typing since the compact: type nothing more — the
                // continue text would land in their draft and send it.
                if self.last_input > at || !self.agent_has_tty() {
                    self.nudge = None;
                    return;
                }
                let text = continue_text(self.project_dir().as_deref());
                let now = Instant::now();
                self.last_content_change = now;
                self.last_nudge = Some(now);
                self.nudge = Some(Nudge::Continued(now));
                self.submit(&text);
            }
            // The agent has answered: arm the next nudge and drop the mark —
            // never before it has been up long enough to be seen, since the
            // echo of the sentence lands within milliseconds of it being typed.
            Some(Nudge::Continued(at)) if at.elapsed() >= NUDGE_MARK && self.last_content_change > at => {
                self.nudge = None;
            }
            _ => {}
        }
    }

    /// May a due nudge read the tty owner now? At most every `SAMPLE_EVERY`.
    fn tty_check_due(&self) -> bool {
        let due = self.tty_checked.get().elapsed() >= SAMPLE_EVERY;
        if due {
            self.tty_checked.set(Instant::now());
        }
        due
    }

    /// Is a nudge due? After the full silence — or, with the context past
    /// `CONTEXT_COMPACT_AT`, after the short one, so the agent is compacted
    /// before it runs out of room rather than after it has been waiting.
    fn nudge_due(&self) -> bool {
        let Some(idle) = self.idle_for() else { return false };
        idle >= IDLE_NUDGE
            || (self.context_full() && idle >= CONTEXT_SETTLE && self.context_compact_allowed())
    }

    /// Is the context full enough for a proactive compaction?
    fn context_full(&self) -> bool {
        self.agent
            .as_ref()
            .and_then(|a| a.context)
            .and_then(Context::fill)
            .is_some_and(|f| f >= CONTEXT_COMPACT_AT)
    }

    /// The usage the probe reads only drops once the agent has answered again
    /// after a compaction, so a context-triggered one is not repeated until
    /// `CONTEXT_COMPACT_EVERY` has passed.
    fn context_compact_allowed(&self) -> bool {
        self.context_compacted.is_none_or(|at| at.elapsed() >= CONTEXT_COMPACT_EVERY)
    }

    /// How long this shell's agent has been waiting for the user — `None`
    /// with no agent, or with one that says it is busy. The client's own
    /// status is exact and preferred; without one the screen hash stands in.
    /// Never longer than since the user last typed here or the auto feature
    /// last did: both start the wait over.
    fn idle_for(&self) -> Option<Duration> {
        self.agent.as_ref()?;
        let waiting = match self.status() {
            Some(Status::Busy) => return None,
            Some(Status::Idle(since)) => SystemTime::now().duration_since(since).unwrap_or_default(),
            None => self.last_content_change.elapsed(),
        };
        let since_nudge = self.last_nudge.map_or(Duration::MAX, |t| t.elapsed());
        Some(waiting.min(self.last_input.elapsed()).min(since_nudge))
    }

    /// The agent's own status, unless it predates what happened here since: an
    /// `idle` stamped before the user's last keystroke or the last nudge was
    /// read before them (background shells are probed only every few seconds),
    /// and trusting it would count a busy agent as long idle. Until the probe
    /// catches up, the screen stands in, as for a client that reports nothing.
    fn status(&self) -> Option<Status> {
        let agent = self.agent.as_ref()?;
        let wall = |t: Instant| SystemTime::now().checked_sub(t.elapsed());
        let newest = self.last_nudge.map_or(self.last_input, |nudge| nudge.max(self.last_input));
        match agent.status? {
            Status::Idle(since) if wall(newest).is_some_and(|event| since < event) => None,
            status => Some(status),
        }
    }

    /// Time left before the auto feature nudges this agent, once the idle
    /// stretch is long enough to be worth showing (kata ai.md: the countdown
    /// in the footer). `None` while busy, mid-nudge, or too early.
    fn countdown(&self) -> Option<Duration> {
        let idle = self.idle_for().filter(|_| self.nudge.is_none())?;
        let due =
            if self.context_full() && self.context_compact_allowed() { CONTEXT_SETTLE } else { IDLE_NUDGE };
        (idle >= COUNTDOWN_FROM.min(due)).then(|| due.saturating_sub(idle))
    }

    /// Has the compaction typed at `at` finished? Yes once the screen changed
    /// after the command and has then stood still for `COMPACT_SETTLE` — or
    /// the client itself reports idle again since the command — or, failing
    /// any change at all, once `COMPACT_TIMEOUT` has passed.
    fn compaction_done(&self, at: Instant) -> bool {
        let changed = self.last_content_change > at;
        let reported = self
            .agent
            .as_ref()
            .and_then(|a| a.status)
            .is_some_and(|s| matches!(s, Status::Idle(since) if SystemTime::now().duration_since(since).unwrap_or_default() < at.elapsed()));
        (changed && self.last_content_change.elapsed() >= COMPACT_SETTLE)
            || reported
            || at.elapsed() >= COMPACT_TIMEOUT
    }

    /// Desktop ping (kata ai.md): an agent in a shell that is not on screen
    /// has gone idle — say so, once per idle stretch, through the host
    /// terminal. An idle stretch that is on screen needs no ping: the user is
    /// looking at it, and it is marked as told so a later tab switch does not
    /// ring for old news.
    fn tick_ping(&mut self, on_screen: bool) {
        let after = if self.status().is_some() { PING_AFTER_STATUS } else { PING_AFTER_SCREEN };
        let idle = self.idle_for().is_some_and(|d| d >= after);
        match (idle, self.pinged) {
            (true, false) => {
                self.pinged = true;
                if !on_screen && let Some(agent) = self.agent.as_ref() {
                    let folder = self.cwd.as_deref().map_or_else(|| "?".into(), folder_name);
                    notify_host(&idle_notice(agent.name, &folder));
                }
            }
            (false, true) => self.pinged = false,
            _ => {}
        }
    }

    /// The project the agent works in — its own working directory, read live
    /// (once per nudge), else the shell's cached one. Where `.ai/auto.md` is
    /// looked for.
    fn project_dir(&self) -> Option<PathBuf> {
        let live = self.agent.as_ref().and_then(|a| std::fs::read_link(format!("/proc/{}/cwd", a.pid)).ok());
        live.or_else(|| self.cwd.clone())
    }

    /// Is this shell's detected agent the process reading the tty right now?
    /// The last gate before the nudge, and read live rather than from the 2 Hz
    /// caches: `agent` comes from a rotating probe and can lag by seconds. Any
    /// other foreground would swallow the sentence — a bare shell prompt would
    /// *run* it as a command, and vim or less would take it as input.
    fn agent_has_tty(&self) -> bool {
        let (Some(agent), Some(pid)) = (self.agent.as_ref(), self.pid) else { return false };
        // The agent's own process group must be the foreground one. That holds
        // for an agent started through a wrapper (it shares the wrapper's
        // group), but not for one suspended under a nested shell: that shell
        // is its ancestor, yet it — not the agent — would read the sentence.
        foreground_pid(pid).is_some_and(|fg| proc_pgrp(agent.pid) == Some(fg))
    }

    /// Start this shell's session transcript once an AI client is detected in
    /// it (kata ai.md). Idempotent and IO-free — the reader thread creates the
    /// file on its first write — so it can simply be called after every probe.
    fn arm_log(&self) {
        let Some(agent) = self.agent.as_ref().filter(|_| !self.log.armed()) else { return };
        let Some(session) = self.log.arm(Meta {
            agent: agent.name.to_string(),
            model: agent.model.clone(),
            cwd: self.cwd.clone().unwrap_or_default(),
            pid: self.pid.unwrap_or_default(),
        }) else {
            return;
        };
        // The reader thread only commits when bytes arrive, and an agent that
        // has answered sends none until it is spoken to again — so its answer
        // would sit uncommitted for as long as the user is reading it. This
        // thread walks that last stretch onto disk (and keeps the sidecar and
        // the fsync off the reader thread), and retires with its session.
        let (log, parser) = (Arc::clone(&self.log), Arc::clone(&self.parser));
        thread::spawn(move || {
            loop {
                thread::sleep(FLUSH_EVERY);
                if !log.serves(session) {
                    break;
                }
                log.pump(&parser, Pump::Flush);
            }
        });
    }

    /// Take the probe's latest answer. An agent that left the shell (or was
    /// replaced by another) ends its transcript — the shell's later output is
    /// no longer the session's — and one detected here starts its own. The
    /// close writes and syncs, so it runs off the UI thread.
    fn set_agent(&mut self, agent: Option<AgentInfo>) {
        let (was, now) = (self.agent.as_ref().map(|a| a.pid), agent.as_ref().map(|a| a.pid));
        self.agent = agent;
        if was.is_some() && was != now {
            let (log, parser) = (Arc::clone(&self.log), Arc::clone(&self.parser));
            thread::spawn(move || log.close(&parser));
        }
        self.arm_log();
    }

    /// Close the transcript with the screen the scrollback never saw. Called
    /// *before* the child is killed: an app that clears the screen on its way
    /// out must not be able to erase the last thing it said. A no-op for a
    /// shell that never ran an agent, and idempotent with the reader thread's
    /// own close at EOF.
    fn finish_log(&self) {
        self.log.close(&self.parser);
    }

    /// Resize the PTY and the screen it is parsed into, in that order and under
    /// one lock. The order matters: resizing the PTY signals the child, which
    /// repaints at the new width immediately, and the reader thread parses that
    /// repaint the moment it lands. Sizing the screen afterwards — or without
    /// the lock — leaves a window in which output meant for the new width is
    /// laid into a grid still on the old one, and the repaint wraps into
    /// garbage that no later frame repairs, because the damage is in the grid.
    /// Holding the lock across both closes the window: the reader cannot take
    /// it until the screen already has the size the child is drawing for.
    fn resize(&mut self, rows: u16, cols: u16) {
        let mut parser = self.parser.lock().unwrap_or_else(PoisonError::into_inner);
        parser.screen_mut().set_size(rows, cols);
        let _ = self.master.resize(pty_size(rows, cols));
        drop(parser);
        self.resized = Instant::now();
    }

    /// Current working directory of the shell, read live from /proc; the UI
    /// uses the `cwd` cache instead, refreshed at 2 Hz by `tick_proc`.
    fn read_cwd(&self) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/cwd", self.pid?)).ok()
    }

    /// Move the host scrollback view by `delta` lines (positive = into older
    /// output); vt100 clamps to the buffer. `delta == 0` snaps back to live.
    /// Returns the rows actually moved — the amount the on-screen text (and
    /// with it any live selection) shifted downward.
    fn scroll(&self, delta: isize) -> isize {
        let mut parser = self.parser.lock().unwrap_or_else(PoisonError::into_inner);
        let screen = parser.screen_mut();
        let before = screen.scrollback();
        let at = if delta == 0 { 0 } else { (before as isize + delta).max(0) as usize };
        screen.set_scrollback(at);
        screen.scrollback() as isize - before as isize
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
    /// Marked with Alt+Shift+F; favorites cluster at the top of the sidebar.
    favorite: bool,
    /// Auto feature (kata ai.md): nudge this tab's idle AI agent to keep
    /// working. Toggled by the tab's `auto` button, off by default, persisted.
    auto: bool,
}

impl Tab {
    /// A new tab: one parent shell in `cwd`, replaying `pending_cmd` if any.
    fn spawn(rows: u16, cols: u16, cwd: &Path, pending_cmd: Option<String>) -> Result<Self, Box<dyn Error>> {
        let parent = Shell::spawn(rows, cols, cwd, pending_cmd)?;
        Ok(Self { shells: vec![parent], active: 0, favorite: false, auto: false })
    }

    /// Auto feature (kata ai.md): with this tab's button on, every agent in it
    /// that has gone quiet is told to keep going — visible or not. With the
    /// button off nothing is ever typed, so the gate lives here, beside the
    /// flag it reads, rather than at the render loop's call site.
    fn nudge_idle_agents(&mut self) {
        if self.auto {
            self.shells.iter_mut().for_each(Shell::nudge_if_idle);
        } else {
            // Switched off mid-nudge: the half-done nudge is abandoned, not
            // resumed hours later when the button goes back on — and its mark
            // must not stay lit on a tab that no longer nudges.
            self.shells.iter_mut().for_each(|shell| shell.nudge = None);
        }
    }

    fn active_shell(&self) -> &Shell {
        &self.shells[self.active]
    }

    fn active_shell_mut(&mut self) -> &mut Shell {
        &mut self.shells[self.active]
    }

    /// Working directory of the active shell — what new tabs/subshells inherit.
    /// Read live: user-triggered and rare, so exactness beats the cache.
    fn cwd(&self) -> Option<PathBuf> {
        self.active_shell().read_cwd()
    }

    /// Sidebar height: a name row and a blank separator bracket every shell's
    /// path + process rows, plus one replay row per shell that is currently
    /// replayable (kata tab) — so a tab grows and shrinks as commands are
    /// captured or programs take the foreground.
    fn rows(&self) -> u16 {
        2 + self.shells.iter().map(|s| 2 + s.replayable() as u16).sum::<u16>()
    }

    /// Cycle the active *shell within this tab* by `delta` (wrapping, Alt+Up/Down);
    /// a no-op with no subshells. Switching *tabs* is `App::navigate_tabs`.
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

impl Drop for Shell {
    /// No shell outlives its owner: any path that drops a `Shell` (closing a
    /// tab, a failed restore, tests) must not leak a live child process. The
    /// reader thread then sees EOF and exits on its own.
    fn drop(&mut self) {
        self.finish_log();
        let _ = self.child.kill();
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

/// A live text selection in the terminal pane. Coordinates are visible-grid
/// (row, col) cells; `shell` binds the selection to the tab+shell it was made
/// in, so it is only drawn (and copied) while that shell is on screen.
#[derive(Clone)]
struct Selection {
    shell: (usize, usize),
    anchor: (u16, u16),
    head: (u16, u16),
    /// True while the left button is held — drag extends `head`; release copies.
    dragging: bool,
    /// Block (rectangular) selection: every cell in the rectangle between
    /// `anchor` and `head`, not reading-order rows. Armed by Shift+Ctrl+drag —
    /// the way to lift a column of text out of a table or a log. A block
    /// selection is copied with each row's columns joined by a newline, so the
    /// clipboard holds exactly the rectangle that was highlighted.
    block: bool,
    /// The copied text, snapshotted the moment the selection was finalized
    /// (drag release, double/triple click, Alt+a). Reading the *live* screen at
    /// copy time is what made the clipboard disagree with the highlight: an
    /// inner app repainting between the last frame and the copy changes what
    /// `contents_between` returns, so text the user never highlighted landed on
    /// the clipboard. Snapshotting at finalize makes the copy exactly the
    /// highlight, whatever the app does afterwards. `None` while still dragging
    /// or for a selection built directly (tests) — then the live screen is read.
    text: Option<String>,
}

/// Agent detection, moved off the render thread. Resolving the model walks
/// every entry in /proc and may open opencode's database: milliseconds of
/// blocking IO that used to land on a frame twice a second. The UI posts the
/// shell pid it wants resolved and picks the answer up whenever it is ready.
struct AgentProbe {
    ask: mpsc::Sender<u32>,
    answers: mpsc::Receiver<(u32, Option<AgentInfo>)>,
    /// A request is out; only one at a time, so a slow probe can never pile up.
    pending: bool,
    asked: Instant,
}

impl AgentProbe {
    fn spawn() -> Self {
        let (ask, requests) = mpsc::channel::<u32>();
        let (reply, answers) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(shell) = requests.recv() {
                let model = detect_agent(shell).map(|(spec, pid)| AgentInfo {
                    name: spec.comm,
                    model: spec
                        .sources
                        .iter()
                        .find_map(|src| resolve_source(src, pid))
                        .unwrap_or_else(|| spec.comm.to_string()),
                    pid,
                    context: resolve_usage(&spec.usage, pid),
                    status: resolve_status(&spec.usage, pid),
                });
                if reply.send((shell, model)).is_err() {
                    break;
                }
            }
        });
        Self { ask, answers, pending: false, asked: stale(SAMPLE_EVERY) }
    }
}

// ── app ──────────────────────────────────────────────────────────────────────

struct App {
    tabs: Vec<Tab>,
    active: usize,
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
    /// Working directory the app was started from — the fallback cwd for new
    /// tabs and subshells when the active shell's directory can't be read
    /// (new shells otherwise inherit the active tab's cwd).
    base: PathBuf,
    /// Last session (folder + command per tab, copy mode) written to disk;
    /// persisted on change.
    saved_session: Session,
    /// Last persistence attempt — persisting runs at 1 Hz off the sampling
    /// caches, not every frame.
    persisted: Instant,
    /// Cached git branch (with the cwd it was read for), refreshed at 2 Hz —
    /// keeps `.git/HEAD` IO out of the per-frame render path.
    branch: Option<String>,
    branch_cwd: Option<PathBuf>,
    branch_sampled: Instant,
    /// Sidebar search row text: filters the tab list as it is typed.
    search: String,
    /// Search row focused: printable keys edit the filter. Focused at app
    /// start and via Alt+f or a click on the row; Esc/Enter or selecting a
    /// tab returns focus to the shell.
    search_focus: bool,
    /// Tab indices passing the search filter, in order — the sidebar renders,
    /// scrolls and hit-tests exactly this list (`list_offset` indexes into
    /// it). Refreshed every loop turn and on every filter edit.
    shown: Vec<usize>,
    /// Rolling cursor into the flat shell list for staggered /proc sampling.
    /// Each frame, a few shells (proportional to total shells) are sampled so
    /// the O(shells) /proc work is spread across the 2-Hz window instead of
    /// landing on one frame — that burst froze the UI with many tabs.
    proc_cursor: usize,
    /// Last shell reaped — reaping runs at 10 Hz, not every frame, since
    /// `try_wait` per shell is a syscall and O(shells) on the hot path.
    reaped: Instant,
    /// Live text selection in the pane (left-drag), copied to the host
    /// clipboard on release. `None` when nothing is selected.
    selection: Option<Selection>,
    /// Copy mode (kata ui.md): ricon owns the pane's left button, so a plain
    /// drag selects text even over an app that grabbed the mouse (vim, less,
    /// opencode) without any modifier trick, and the middle button pastes. On
    /// by default; the footer button or Alt+c switches it off — then the whole
    /// mouse goes to the app — and the choice is persisted with the session.
    copy_mode: bool,
    /// Text read back from the desktop clipboard for a middle-click paste.
    /// The read runs on the clipboard thread (it can block on a dead selection
    /// owner), and lands here for the loop to type into the active shell.
    pastes: (mpsc::Sender<String>, mpsc::Receiver<String>),
    /// Time and cell of the last pane click, with its position in a
    /// double/triple-click chain — the timing a terminal has to reconstruct
    /// itself, since hosts only ever report single presses.
    last_click: Option<(Instant, (u16, u16), u8)>,
    /// A mouse press was consumed by ricon (closing the cheat sheet): the
    /// drag and release that belong to it are dropped too.
    swallow_release: bool,
    /// When the last copy landed; a brief "✓ copied" footer hint shows while
    /// this is within `COPY_HINT` of now. `None` until the first copy.
    copied_at: Option<Instant>,
    /// Off-thread AI-agent/model resolution, rotating over the shells.
    agent_probe: AgentProbe,
    /// Rolling cursor into that rotation: the on-screen shell (whose model the
    /// status bar draws) alternates with one background shell at a time, so
    /// every tab still gets an agent resolved for the auto-continue nudge.
    agent_cursor: usize,
    /// Last frame drawn — the app renders at most once per `POLL_INTERVAL` and
    /// spends the rest of the budget draining input.
    drawn: Instant,
    /// Set by confirming the quit dialog; the loop then shuts every shell
    /// down and exits.
    quit: bool,
    /// Quit-confirmation dialog: `None` when closed, `Some(yes)` when open
    /// with YES (`true`) or NO (`false`) highlighted. Alt+q opens it with NO
    /// preselected so a stray press can't kill every tab.
    confirm_quit: Option<bool>,
    /// The shortcut cheat sheet (Alt+?) is up; any key or click takes it down.
    help: bool,
}

/// One persisted shell: its folder and the command running in it (if any).
#[derive(Clone, PartialEq, Debug)]
struct ShellState {
    cwd: PathBuf,
    cmd: Option<String>,
}

/// One persisted tab: its shells (parent first, then subshells in order),
/// which shell was active, whether it was the active tab at save time,
/// whether it was marked as a favorite, and its auto-feature state.
#[derive(Clone, PartialEq, Debug)]
struct TabState {
    shells: Vec<ShellState>,
    active_shell: usize,
    active: bool,
    favorite: bool,
    auto: bool,
}

/// Everything the session file holds: the tabs, and the app-wide copy mode
/// (on unless it was switched off — a fresh install starts with it on).
#[derive(Clone, PartialEq, Debug)]
struct Session {
    tabs: Vec<TabState>,
    copy_mode: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self { tabs: Vec::new(), copy_mode: true }
    }
}

impl App {
    fn new() -> Result<Self, Box<dyn Error>> {
        let base = base_path(std::env::args().nth(1))?;
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
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
            saved_session: Session::default(),
            persisted: Instant::now(),
            branch: None,
            branch_cwd: None,
            branch_sampled: Instant::now(),
            search: String::new(),
            search_focus: true,
            shown: Vec::new(),
            proc_cursor: 0,
            reaped: Instant::now(),
            selection: None,
            copy_mode: true,
            pastes: mpsc::channel(),
            last_click: None,
            swallow_release: false,
            copied_at: None,
            agent_probe: AgentProbe::spawn(),
            agent_cursor: 0,
            drawn: stale(POLL_INTERVAL),
            quit: false,
            confirm_quit: None,
            help: false,
        };
        // Until v0.3 the auto feature was app-global in a flag file beside the
        // session; it is per-tab inside the session now — sweep the orphan so
        // stale state doesn't linger.
        if let Some(flag) = session_path().map(|p| p.with_file_name("auto")) {
            let _ = std::fs::remove_file(flag);
        }
        // Restore the persisted tabs at their saved folders, replaying each
        // tab's recorded command; fall back to a single base-path shell when
        // there is no (still-valid) session.
        // Drop shells whose folder no longer exists; a tab left with none is
        // dropped entirely.
        let session = load_session();
        app.copy_mode = session.copy_mode;
        let restored: Vec<TabState> = session
            .tabs
            .into_iter()
            .filter_map(|mut t| {
                t.shells.retain(|s| s.cwd.is_dir());
                (!t.shells.is_empty()).then_some(t)
            })
            .collect();
        if restored.is_empty() {
            app.open_tab()?;
        } else {
            // A folder that exists but can no longer be entered fails its
            // spawn: that tab is skipped, never the whole session.
            let mut active = 0;
            for state in &restored {
                if app.restore_tab(state).is_ok() && state.active {
                    active = app.tabs.len() - 1;
                }
            }
            app.active = active;
            if app.tabs.is_empty() {
                app.open_tab()?;
            }
        }
        app.refresh_shown();
        Ok(app)
    }

    /// Rebuild the filtered tab list: indices whose folder path contains the
    /// search text (case-insensitive); every tab when the search is empty.
    fn refresh_shown(&mut self) {
        let needle = self.search.to_lowercase();
        self.shown = (0..self.tabs.len())
            .filter(|&i| {
                needle.is_empty()
                    || self.tabs[i].shells[0]
                        .cwd
                        .as_deref()
                        .is_some_and(|p| p.display().to_string().to_lowercase().contains(&needle))
            })
            .collect();
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<(), Box<dyn Error>> {
        loop {
            // Reap at 10 Hz: `try_wait` is a syscall per shell, so the per-frame
            // O(shells) sweep is throttled off the 30-ms render path.
            if self.reaped.elapsed() >= Duration::from_millis(100) {
                self.reaped = Instant::now();
                self.reap_dead_tabs();
            }
            if self.quit {
                // Graceful exit: persist the final state (the 1 Hz throttle
                // may be up to a second behind), stop every shell, then unwind
                // to main() which restores the host terminal (mouse, paste,
                // keyboard flags).
                self.persist_now();
                for tab in &mut self.tabs {
                    for shell in &mut tab.shells {
                        shell.finish_log();
                        let _ = shell.child.kill();
                    }
                }
                return Ok(());
            }
            if self.tabs.is_empty() {
                // The last shell was closed: persist the now-empty session so
                // the next start opens fresh instead of resurrecting the tab
                // the user just closed (the 1 Hz snapshot still contains it).
                self.persist_now();
                return Ok(());
            }
            // Everything below is per frame, not per wake: a burst of events
            // wakes the loop far more often than it draws, and the /proc
            // sampling and ticks are budgeted for one pass a frame.
            if self.drawn.elapsed() >= POLL_INTERVAL {
                // The filtered tab list drives everything below (reveal, render,
                // hit-testing); tabs may have been added, closed or reaped.
                self.refresh_shown();
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
                        let on_screen = ti == active_tab && si == shown;
                        shell.tick_activity(on_screen);
                        shell.tick_ping(on_screen);
                        shell.flush_pending();
                    }
                    tab.nudge_idle_agents();
                }
                // Staggered /proc sampling: a slice of shells each frame so the 3N
                // /proc reads (cwd, stat, cmdline per shell) spread across the
                // 2-Hz window instead of one O(shells) burst frame — that burst
                // froze the UI with many tabs.
                self.sample_shells();
                // Only the shell whose status bar is on screen needs its AI agent
                // resolved, and never on this thread — see `AgentProbe`.
                self.tick_agent();
                // A middle-click paste whose clipboard read has come back.
                while let Ok(text) = self.pastes.1.try_recv() {
                    self.on_paste(&text)?;
                }
                self.persist_session();
                self.fit_ptys(terminal.size()?.into());
                self.drop_stale_selection();
                terminal.draw(|frame| draw(frame, self))?;
                self.drawn = Instant::now();
            }
            // Spend the rest of the frame waiting for input, then handle
            // everything the host has already queued before drawing again. A
            // burst (mouse motion under an any-motion app, a large paste) then
            // costs one frame instead of one full render per event — rendering
            // per event is what made ricon freeze under the cursor.
            if !event::poll(POLL_INTERVAL.saturating_sub(self.drawn.elapsed()))? {
                continue;
            }
            let deadline = Instant::now() + DRAIN_BUDGET;
            loop {
                self.handle(event::read()?)?;
                // Stop draining when the app is going away (the state the rest
                // of the loop relies on is gone), when the budget is spent, or
                // when the queue runs dry.
                if self.quit
                    || self.tabs.is_empty()
                    || Instant::now() >= deadline
                    || !event::poll(Duration::ZERO)?
                {
                    break;
                }
            }
        }
    }

    fn handle(&mut self, event: Event) -> Result<(), Box<dyn Error>> {
        let handled = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Mouse(mouse) => self.on_mouse(mouse),
            Event::Paste(text) => self.on_paste(&text),
            _ => Ok(()),
        };
        // Right here, not once a frame: an event that switched tab or shell
        // must not leave a selection the next event in the same drain could
        // still copy from.
        self.drop_stale_selection();
        handled
    }

    /// Collect any resolved agent, then ask for the next shell's. Both halves
    /// are non-blocking; the answer lands a frame or two later. The on-screen
    /// shell is asked every other turn (its model is what the status bar
    /// draws), the background shells take the turns in between — the auto
    /// feature needs an agent resolved in every tab, not just the visible one.
    fn tick_agent(&mut self) {
        while let Ok((pid, agent)) = self.agent_probe.answers.try_recv() {
            self.agent_probe.pending = false;
            for shell in self.tabs.iter_mut().flat_map(|t| t.shells.iter_mut()) {
                if shell.pid == Some(pid) {
                    shell.set_agent(agent.clone());
                }
            }
        }
        if self.agent_probe.pending || self.agent_probe.asked.elapsed() < SAMPLE_EVERY {
            return;
        }
        let Some(pid) = self.next_probe_target() else { return };
        self.agent_probe.asked = Instant::now();
        self.agent_probe.pending = self.agent_probe.ask.send(pid).is_ok();
    }

    /// Next shell pid to resolve an agent for: the on-screen one on even turns,
    /// the next shell in the flat rotation on odd ones.
    fn next_probe_target(&mut self) -> Option<u32> {
        let on_screen = self.tabs.get(self.active).and_then(|t| t.active_shell().pid);
        self.agent_cursor = self.agent_cursor.wrapping_add(1);
        if self.agent_cursor.is_multiple_of(2) && on_screen.is_some() {
            return on_screen;
        }
        let pids: Vec<u32> = self.tabs.iter().flat_map(|t| t.shells.iter()).filter_map(|s| s.pid).collect();
        pids.get(self.agent_cursor / 2 % pids.len().max(1)).copied().or(on_screen)
    }

    /// Toggle a tab's auto feature and persist the choice at once (kata ai.md).
    fn toggle_auto(&mut self, index: usize) {
        self.tabs[index].auto = !self.tabs[index].auto;
        self.persist_now();
    }

    /// A selection belongs to the shell it was made in: switching tab or shell
    /// drops it, so the copy button can never reach for text nobody can see.
    fn drop_stale_selection(&mut self) {
        let on_screen = self.tabs.get(self.active).map(|tab| (self.active, tab.active));
        if self.selection.as_ref().is_some_and(|sel| Some(sel.shell) != on_screen) {
            self.selection = None;
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<(), Box<dyn Error>> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // The quit dialog is modal: arrows toggle YES/NO, Enter confirms,
        // Esc cancels; every other key is swallowed so nothing leaks to the
        // shell underneath.
        if let Some(yes) = self.confirm_quit {
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                    self.confirm_quit = Some(!yes);
                }
                KeyCode::Enter => {
                    self.quit = yes;
                    self.confirm_quit = None;
                }
                KeyCode::Esc => self.confirm_quit = None,
                _ => {}
            }
            return Ok(());
        }
        // The cheat sheet is modal too: the next key, whatever it is, only
        // takes it down.
        if self.help {
            self.help = false;
            return Ok(());
        }
        // Every arm below reads the active tab; with none left the loop is
        // already on its way out.
        if self.tabs.is_empty() {
            return Ok(());
        }
        // Every shortcut of ricon's own lives on Alt (kata app.md), so Ctrl
        // keys reach the shell untouched — Ctrl+C is always SIGINT, never a
        // copy (releasing the drag already copied). Esc is never shadowed
        // either: with a selection live, both merely take the highlight down
        // on their way through.
        // Ctrl+Alt+letter is the app's (Emacs/readline C-M- bindings): only a
        // bare Alt chord is ricon's.
        let ours = alt && !ctrl;
        match key.code {
            // Alt+c toggles copy mode from the keyboard (the footer button does
            // the same with the mouse); Alt+a selects the whole visible screen
            // and copies it — "copy everything", but shown before it is taken.
            KeyCode::Char('c' | 'C') if ours => self.toggle_copy_mode(),
            KeyCode::Char('a' | 'A') if ours => self.select_all(),
            KeyCode::Char('?' | 'h' | 'H') if ours => self.help = true,
            KeyCode::Char('q' | 'Q') if ours => self.confirm_quit = Some(false),
            // A shell that fails to spawn (a folder gone unenterable) opens
            // nothing; it must never take every other tab down with it.
            KeyCode::Char('t' | 'T' | 'n' | 'N') if ours => self.open_tab().unwrap_or_default(),
            KeyCode::Char('w' | 'W') if ours => self.close_active(),
            // Alt+f focuses the search row; the favorite toggle, which shared
            // the letter, moved to Alt+Shift+f.
            KeyCode::Char('f') if ours => self.search_focus = true,
            KeyCode::Char('F') if ours => self.toggle_favorite(),
            KeyCode::Char('s' | 'S') if ours => self.open_subshell().unwrap_or_default(),
            KeyCode::Char('r' | 'R') if ours => self.tabs[self.active].active_shell_mut().replay(),
            KeyCode::Up if ours => self.tabs[self.active].navigate(-1),
            KeyCode::Down if ours => self.tabs[self.active].navigate(1),
            // Tab-switching stays within the search-filtered list (`shown`):
            // Alt+N jumps to the Nth visible tab, Alt+PageUp/Down step between
            // visible tabs — hidden (filtered-out) tabs are skipped.
            KeyCode::Char(c @ '1'..='9') if ours => {
                if let Some(&index) = self.shown.get(c as usize - '1' as usize) {
                    self.active = index;
                }
            }
            KeyCode::PageDown if ours => self.navigate_tabs(1),
            KeyCode::PageUp if ours => self.navigate_tabs(-1),
            // Search row editing while it has focus: printable keys build the
            // filter, Backspace erases, Esc/Enter hand focus back to the shell.
            KeyCode::Esc | KeyCode::Enter if self.search_focus => self.search_focus = false,
            KeyCode::Backspace if self.search_focus => {
                self.search.pop();
                self.refresh_shown();
            }
            KeyCode::Char(c) if self.search_focus && !ctrl && !alt => {
                self.search.push(c);
                self.refresh_shown();
            }
            _ => {
                if matches!(key.code, KeyCode::Esc) || (ctrl && matches!(key.code, KeyCode::Char('c'))) {
                    self.selection = None; // the highlight goes; the key still goes through
                }
                let Some(tab) = self.tabs.get(self.active) else { return Ok(()) };
                let modes = tab.active_shell().modes();
                if let Some(bytes) = encode_key(&key, modes.app_cursor) {
                    self.write_active(&bytes);
                }
            }
        }
        Ok(())
    }

    /// Switch copy mode (kata ui.md) and persist the choice at once, like the
    /// auto button does. A selection made under the old setting is dropped.
    fn toggle_copy_mode(&mut self) {
        self.copy_mode = !self.copy_mode;
        self.selection = None;
        self.persist_now();
    }

    /// Middle click: paste the desktop's primary selection into the active
    /// shell, the way every terminal does — ricon holds the mouse, so the host
    /// terminal cannot do it on its behalf. The clipboard is read off-thread
    /// and the text lands via `pastes` a frame later.
    fn request_paste(&self) {
        desktop_clipboard_get(self.pastes.0.clone());
    }

    /// Switch the active tab by `delta` visible positions (wrapping) within the
    /// search-filtered list, so navigation skips filtered-out tabs. A no-op when
    /// nothing passes the filter.
    fn navigate_tabs(&mut self, delta: isize) {
        if self.shown.is_empty() {
            return;
        }
        let pos = self.shown.iter().position(|&i| i == self.active).unwrap_or(0);
        let next = (pos as isize + delta).rem_euclid(self.shown.len() as isize) as usize;
        self.active = self.shown[next];
    }

    fn on_paste(&mut self, text: &str) -> Result<(), Box<dyn Error>> {
        let Some(tab) = self.tabs.get(self.active).filter(|_| self.confirm_quit.is_none()) else {
            return Ok(());
        };
        let text = paste_text(text);
        if tab.active_shell().modes().bracketed_paste {
            self.write_active(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.write_active(text.as_bytes());
        }
        Ok(())
    }

    /// Sidebar rows available for tabs (row 0 is the " ricon " title, row 1
    /// the search row).
    fn viewport_rows(&self) -> usize {
        self.sidebar_rows.saturating_sub(2) as usize
    }

    /// Total rows every shown tab would occupy — tabs have variable height
    /// (four rows plus two per subshell), so this is a sum, not a count × 4.
    fn content_rows(&self) -> usize {
        self.shown.iter().map(|&i| self.tabs[i].rows() as usize).sum()
    }

    /// Tab index under sidebar row `row`: walk shown-tab heights from the
    /// current scroll offset. `None` for the title row (0), the search row (1)
    /// or rows past the last shown tab.
    fn tab_at_row(&self, row: u16) -> Option<usize> {
        self.hit(row).map(|(tab, _, _)| tab)
    }

    /// Tab and shell index under sidebar row `row` — the name row and the blank
    /// separator map to the parent; otherwise the shell whose path/process/replay
    /// block was hit.
    fn shell_at_row(&self, row: u16) -> Option<(usize, usize)> {
        self.hit(row).map(|(tab, shell, _)| (tab, shell))
    }

    /// Resolve sidebar row `row` against the shown tabs: the tab, the shell whose
    /// rows were hit, and whether it was that shell's replay row (where the icon
    /// lives). `None` above the tabs or past the last one. Walks each shell's
    /// block — path, process, and a replay row while replayable — so it tracks
    /// the same variable height `Tab::rows` reports.
    fn hit(&self, row: u16) -> Option<(usize, usize, bool)> {
        let mut r = (row as usize).checked_sub(2)?;
        for (&i, h) in self.drawn_tabs() {
            let tab = &self.tabs[i];
            if r < h {
                // Local row 0 is the tab name and the last row is the blank
                // separator; both belong to the parent and carry no icon.
                if r == 0 || r + 1 == h {
                    return Some((i, 0, false));
                }
                // The rows between are each shell's block: path, process, then a
                // replay row (icon + command) while the shell is replayable.
                let mut local = r - 1;
                for (si, shell) in tab.shells.iter().enumerate() {
                    let block = 2 + shell.replayable() as usize;
                    if local < block {
                        return Some((i, si, shell.replayable() && local == 2));
                    }
                    local -= block;
                }
                return Some((i, 0, false)); // unreachable: the blocks fill h - 2
            }
            r -= h;
        }
        None
    }

    /// The (tab, shell) whose `replay` button (`row`, `col`) hits: a replay row
    /// (which `hit` only reports for a replayable shell) clicked anywhere from
    /// the icon to the end of the command it re-runs — the whole `🔁 <command>`
    /// stretch is the button, so a click needn't land on the two-cell emoji.
    fn replay_at(&self, row: u16, col: u16) -> Option<(usize, usize)> {
        let (tab, shell, replay_row) = self.hit(row)?;
        (replay_row
            && replay_span(&self.tabs[tab].shells[shell], self.sidebar_width).contains(&(col as usize)))
        .then_some((tab, shell))
    }

    /// The tab whose `auto` button (`row`, `col`) hits: the button sits on a
    /// tab's first (name) row, after its text and indicators (kata ai.md).
    /// Walks the same shown-tab heights as `hit`.
    fn auto_at(&self, row: u16, col: u16) -> Option<usize> {
        let mut r = (row as usize).checked_sub(2)?;
        for (&i, h) in self.drawn_tabs() {
            if r == 0 {
                return auto_button(i, &self.tabs[i], self.sidebar_width)
                    .is_some_and(|span| span.contains(&(col as usize)))
                    .then_some(i);
            }
            if r < h {
                return None; // inside this tab, but not its name row
            }
            r -= h;
        }
        None
    }

    /// Shown tabs intersecting the sidebar viewport from the current scroll
    /// offset, as a range of positions into `shown`. The render builds items
    /// only for these — with many tabs, building every item every frame is
    /// wasted work (and froze the UI at scale).
    fn visible_tabs(&self) -> std::ops::Range<usize> {
        let start = self.list_offset.min(self.shown.len());
        start..start + self.drawn_tabs().count()
    }

    /// The shown tabs the sidebar actually draws, with their heights: from the
    /// scroll offset, every tab that fits the viewport *whole*. The list widget
    /// skips a tab it can only draw part of, so the hit-tests walk exactly
    /// these — a click on the blank rows below must never reach a hidden tab.
    fn drawn_tabs(&self) -> impl Iterator<Item = (&usize, usize)> {
        let vp = self.viewport_rows();
        let mut used = 0;
        self.shown.get(self.list_offset..).unwrap_or_default().iter().map_while(move |i| {
            let h = self.tabs[*i].rows() as usize;
            used += h;
            (used <= vp).then_some((i, h))
        })
    }

    /// Largest first-visible position that still fills the viewport — the
    /// clamp for any scroll. Zero when every shown tab already fits.
    fn max_offset(&self) -> usize {
        let vp = self.viewport_rows();
        let mut used = 0;
        let mut i = self.shown.len();
        while i > 0 && used + self.tabs[self.shown[i - 1]].rows() as usize <= vp {
            used += self.tabs[self.shown[i - 1]].rows() as usize;
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
    /// when it is already visible or filtered out by the search.
    fn reveal_active(&mut self) {
        let Some(pos) = self.shown.iter().position(|&i| i == self.active) else { return };
        if pos < self.list_offset {
            self.list_offset = pos;
            return;
        }
        // Scroll down just enough that the active tab's last row is on screen.
        let vp = self.viewport_rows();
        while self.list_offset < pos {
            let used: usize =
                self.shown[self.list_offset..=pos].iter().map(|&i| self.tabs[i].rows() as usize).sum();
            if used <= vp {
                break;
            }
            self.list_offset += 1;
        }
    }

    /// Sidebar-border drags resize the panel; everything else over the
    /// terminal pane is forwarded to the inner app (when it asked for mouse).
    fn on_mouse(&mut self, mouse: MouseEvent) -> Result<(), Box<dyn Error>> {
        // The quit dialog is modal: no clicking through it into the sidebar
        // or the terminal pane. So is the cheat sheet — a click takes it down.
        if self.help {
            self.help = !matches!(mouse.kind, MouseEventKind::Down(_));
            self.swallow_release = !self.help;
            return Ok(());
        }
        // The press that closed the cheat sheet was ricon's, so its drag and
        // release are too: an app must never see a release without a press.
        if self.swallow_release {
            self.swallow_release = !matches!(mouse.kind, MouseEventKind::Up(_));
            if matches!(mouse.kind, MouseEventKind::Up(_) | MouseEventKind::Drag(_)) {
                return Ok(());
            }
        }
        if self.confirm_quit.is_some() || self.tabs.is_empty() {
            return Ok(());
        }
        // The footer's button (just left of the version corner) toggles copy mode.
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && mouse.row == self.pty_rows
            && copy_button_x(self.term_width).is_some_and(|(from, to)| (from..to).contains(&mouse.column))
        {
            self.toggle_copy_mode();
            return Ok(());
        }
        // A live left-drag selection owns the mouse: extend it on drag (clamped
        // to the pane grid) and copy to the clipboard on release. A release with
        // no movement is a plain click, which just clears the selection.
        let (sw, pr, pc) = (self.sidebar_width, self.pty_rows, self.pty_cols);
        match (self.selection.as_mut(), mouse.kind) {
            (Some(sel), MouseEventKind::Drag(MouseButton::Left)) if sel.dragging => {
                sel.head = (
                    mouse.row.min(pr.saturating_sub(1)),
                    mouse.column.saturating_sub(sw).min(pc.saturating_sub(1)),
                );
                // Shift+Ctrl held through the drag makes it a block selection;
                // the flag is read live so the user can switch mid-gesture.
                sel.block = mouse.modifiers.contains(KeyModifiers::SHIFT)
                    && mouse.modifiers.contains(KeyModifiers::CONTROL);
                // A drag is not a click: it ends any double/triple-click chain,
                // so pressing again where a drag started selects afresh instead
                // of surprising the user with the word under the cursor.
                self.last_click = None;
                return Ok(());
            }
            (Some(sel), MouseEventKind::Up(MouseButton::Left)) if sel.dragging => {
                sel.dragging = false;
                let sel = sel.clone();
                if sel.anchor == sel.head {
                    self.selection = None;
                } else {
                    // Finalize: snapshot the text now, so the copy is exactly
                    // the highlight even if the inner app repaints before the
                    // clipboard write lands.
                    let text = self.selection_text(sel.clone());
                    let finalized = Selection { text, ..sel };
                    self.flash_copy(self.copy_selection(finalized.clone()));
                    self.selection = Some(finalized);
                }
                return Ok(());
            }
            _ => {}
        }
        // The resize grab zone is the border column and the one left of it —
        // both inside the sidebar. It must never reach into the pane: a zone
        // one column wide there swallowed every selection started at the pane's
        // left edge, and turned it into a sidebar drag instead.
        // A tab's `auto` button is glued to that same right edge, so it wins on
        // the cells it actually occupies — a click on a visible button must
        // toggle it, never start a resize; the border column beside it still
        // grabs, on that row like on every other.
        let grab = self.sidebar_width.saturating_sub(2)..self.sidebar_width;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if grab.contains(&mouse.column) && self.auto_at(mouse.row, mouse.column).is_none() =>
            {
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
            // Clicks inside the sidebar: a tab's `auto` button toggles its
            // continue feature; the `replay` button re-runs that shell's last
            // command; the search row takes focus; a tab entry selects that
            // terminal and the specific shell whose row was hit, and arms the
            // tab for drag-reordering.
            MouseEventKind::Down(MouseButton::Left) if mouse.column < self.sidebar_width => {
                self.selection = None;
                if let Some(index) = self.auto_at(mouse.row, mouse.column) {
                    self.toggle_auto(index);
                } else if let Some((index, shell)) = self.replay_at(mouse.row, mouse.column) {
                    self.active = index;
                    self.tabs[index].active = shell;
                    self.search_focus = false;
                    self.tabs[index].shells[shell].replay();
                } else if mouse.row == 1 {
                    self.search_focus = true;
                } else if let Some((index, shell)) = self.shell_at_row(mouse.row)
                    && index < self.tabs.len()
                {
                    self.active = index;
                    self.tabs[index].active = shell;
                    self.dragging_tab = Some(index);
                    self.search_focus = false;
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
                    // `shown` holds indices into `tabs`, which just moved under
                    // it. One drag produces many events and a whole burst of
                    // them is drained between two frames, so leaving it stale
                    // until the next render made every event after the first
                    // resolve its row against the old order — with a search
                    // filter on (`shown` sparse, so reordering really does
                    // renumber it) a single drag scattered tabs at random.
                    self.refresh_shown();
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
            let grabbed = modes.mouse_mode != MouseProtocolMode::None;
            // Copy mode and the Alt (Shift) bypass both hand the pane's left
            // button to ricon even over an app that grabbed the mouse — the way
            // to select console text out of vim, less, htop or a coding agent.
            // Alt is the modifier to reach for: host terminals (VTE, xterm,
            // kitty, alacritty…) keep Shift+drag for their own window-wide
            // selection, so those events never arrive here and the host
            // highlights whole rows — sidebar tab text included. Copy mode
            // needs no modifier at all, and it is on by default; only the left
            // button (and the middle one, for paste) is taken — the wheel and
            // the rest still reach an app that asked for the mouse, so it
            // keeps scrolling its own content. Shift+Ctrl+drag is a block
            // (rectangular) selection — the column of text a table or a log
            // needs — and is owned by ricon too.
            let ours = !grabbed || self.copy_mode || mouse.modifiers.intersects(BYPASS);
            // Wheel over the pane scrolls this shell's scrollback unless the
            // inner app subscribed to the mouse (then it is forwarded, so
            // less/vim/opencode scroll as usual).
            if !grabbed {
                let step = match mouse.kind {
                    MouseEventKind::ScrollUp => SCROLL_STEP as isize,
                    MouseEventKind::ScrollDown => -(SCROLL_STEP as isize),
                    _ => 0,
                };
                if step != 0 {
                    self.scroll_pane(step);
                    return Ok(());
                }
            }
            if ours {
                match mouse.kind {
                    // One click starts a drag selection, two take the word under
                    // the cursor, three the whole (soft-wrap-joined) line — the
                    // gestures every terminal has, reconstructed from timing
                    // because hosts only ever report single presses.
                    MouseEventKind::Down(MouseButton::Left) => {
                        let cell = (mouse.row, mouse.column - self.sidebar_width);
                        let shell = (self.active, self.tabs[self.active].active);
                        match self.click_count(cell) {
                            2 => self.select_span(shell, self.word_at(cell)),
                            3 => self.select_span(shell, self.line_at(cell)),
                            _ => {
                                let block = mouse.modifiers.contains(KeyModifiers::SHIFT)
                                    && mouse.modifiers.contains(KeyModifiers::CONTROL);
                                self.selection = Some(Selection {
                                    shell,
                                    anchor: cell,
                                    head: cell,
                                    dragging: true,
                                    block,
                                    text: None,
                                });
                            }
                        }
                        return Ok(());
                    }
                    // The other half of a gesture ricon owns is never forwarded.
                    MouseEventKind::Up(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left) => {
                        return Ok(());
                    }
                    // The middle button pastes; its release is nothing.
                    MouseEventKind::Down(MouseButton::Middle) => {
                        self.request_paste();
                        return Ok(());
                    }
                    MouseEventKind::Up(MouseButton::Middle) | MouseEventKind::Drag(MouseButton::Middle) => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
            let (col, row) = (mouse.column - self.sidebar_width, mouse.row);
            if let Some(bytes) = encode_mouse(&mouse, col, row, &modes) {
                self.forward(&bytes);
            }
        }
        Ok(())
    }

    /// Scroll the active pane and carry a live selection along with the text it
    /// covers; the part scrolled out of view is let go (the grid is all a
    /// selection addresses), and the whole of it once none is left in view.
    fn scroll_pane(&mut self, delta: isize) {
        let moved = self.tabs[self.active].active_shell().scroll(delta);
        let (rows, last_col) = (self.pty_rows as isize, self.pty_cols.saturating_sub(1));
        if moved == 0 {
            return;
        }
        if let Some(sel) = self.selection.as_ref() {
            let (anchor, head) = (sel.anchor.0 as isize + moved, sel.head.0 as isize + moved);
            let gone = |r: isize| r < 0 || r >= rows;
            // An endpoint pushed past an edge pins to that edge's far corner —
            // keeping its column would cut the rows it still covers short.
            let pin = |r: isize, col: u16| match r {
                _ if r < 0 => (0, 0),
                _ if r >= rows => ((rows - 1) as u16, last_col),
                _ => (r as u16, col),
            };
            self.selection = (!(gone(anchor) && gone(head))).then(|| Selection {
                anchor: pin(anchor, sel.anchor.1),
                head: pin(head, sel.head.1),
                // The coordinates moved, so a finalized snapshot no longer
                // matches them — drop it and re-read the live screen on copy.
                text: None,
                ..sel.clone()
            });
        }
    }

    /// Position of this click in a double/triple-click chain (1, 2 or 3):
    /// clicks repeat on the same cell within `MULTI_CLICK` to chain, and the
    /// count wraps so a fourth click starts over.
    fn click_count(&mut self, cell: (u16, u16)) -> u8 {
        let now = Instant::now();
        let count = match self.last_click {
            Some((at, was, n)) if was == cell && now.duration_since(at) < MULTI_CLICK => n % 3 + 1,
            _ => 1,
        };
        self.last_click = Some((now, cell, count));
        count
    }

    /// Grid span of the word under `cell` (double click).
    fn word_at(&self, cell: (u16, u16)) -> ((u16, u16), (u16, u16)) {
        let chars = self.tabs[self.active].active_shell().row_chars(cell.0, self.pty_cols);
        let (from, to) = word_span(&chars, cell.1);
        ((cell.0, from), (cell.0, to))
    }

    /// Grid span of the whole logical line under `cell` (triple click).
    fn line_at(&self, cell: (u16, u16)) -> ((u16, u16), (u16, u16)) {
        let (first, last) = self.tabs[self.active].active_shell().logical_line(cell.0, self.pty_rows);
        ((first, 0), (last, self.pty_cols.saturating_sub(1)))
    }

    /// Open a new shell right after the active tab, starting in the active
    /// tab's working directory (falling back to the base path when there is no
    /// active tab or its cwd can't be read), with no command to replay.
    fn open_tab(&mut self) -> Result<(), Box<dyn Error>> {
        let cwd = self.tabs.get(self.active).and_then(Tab::cwd).unwrap_or_else(|| self.base.clone());
        let tab = Tab::spawn(self.pty_rows, self.pty_cols, &cwd, None)?;
        // Right after the active tab, but never inside the favorites block: a
        // new (non-favorite) tab lands after the last favorite, whichever comes
        // later, keeping favorites contiguous at the top.
        let after_favorites = self.tabs.iter().take_while(|t| t.favorite).count();
        let at = (self.active + 1).max(after_favorites).min(self.tabs.len());
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

    /// Toggle the active tab's favorite flag and re-cluster it: favorites form
    /// a contiguous block at the top of the sidebar in marking order, so a
    /// newly-marked tab lands right after the last favorite and an unmarked one
    /// drops just below that block. The active selection follows the tab.
    fn toggle_favorite(&mut self) {
        if self.active >= self.tabs.len() {
            return;
        }
        let mut tab = self.tabs.remove(self.active);
        tab.favorite = !tab.favorite;
        let dest = self.tabs.iter().take_while(|t| t.favorite).count();
        self.tabs.insert(dest, tab);
        self.active = dest;
    }

    /// Append a restored tab at the end (restore preserves saved order): spawn
    /// the parent shell, then each persisted subshell, and select the shell
    /// that was active at save time. `state.shells` is guaranteed non-empty.
    fn restore_tab(&mut self, state: &TabState) -> Result<(), Box<dyn Error>> {
        let (rows, cols) = (self.pty_rows, self.pty_cols);
        let mut shells = state.shells.iter();
        let parent = shells.next().expect("restored tab has at least one shell");
        let mut tab = Tab::spawn(rows, cols, &parent.cwd, parent.cmd.clone())?;
        for sub in shells {
            tab.shells.push(Shell::spawn(rows, cols, &sub.cwd, sub.cmd.clone())?);
        }
        tab.active = state.active_shell.min(tab.shells.len() - 1);
        tab.favorite = state.favorite;
        tab.auto = state.auto;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    /// Persist every open tab's folder and running command so the session
    /// reopens as-is, at 1 Hz (quit forces a final write). Building the state
    /// is IO-free — it snapshots the 2 Hz `tick_proc` caches.
    fn persist_session(&mut self) {
        const PERSIST_EVERY: Duration = Duration::from_secs(1);
        if self.persisted.elapsed() < PERSIST_EVERY {
            return;
        }
        self.persisted = Instant::now();
        self.persist_now();
    }

    /// Snapshot the session and write it to disk; writes only on change.
    fn persist_now(&mut self) {
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabState {
                // Persist every shell in the tab — parent first, then subshells.
                shells: t
                    .shells
                    .iter()
                    .map(|s| ShellState {
                        cwd: s.cwd.clone().unwrap_or_else(|| self.base.clone()),
                        cmd: s.fg_cmd.clone(),
                    })
                    .collect(),
                active_shell: t.active,
                active: i == self.active,
                favorite: t.favorite,
                auto: t.auto,
            })
            .collect();
        let session = Session { tabs, copy_mode: self.copy_mode };
        if session != self.saved_session {
            save_session(&session);
            self.saved_session = session;
        }
    }

    /// Close the active shell of the active tab; when it was the tab's last
    /// shell, the tab itself closes on the next reap. Reap immediately so the
    /// close is visible without waiting on the 10-Hz throttle.
    fn close_active(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let _ = tab.active_shell_mut().child.kill();
        }
        self.reaped = stale(Duration::from_secs(1));
        self.reap_dead_tabs();
    }

    /// Sample /proc for a slice of shells this frame, rotating through all of
    /// them over the 2-Hz window: every shell (the active one included) is
    /// sampled every ~`SAMPLE_EVERY`, but only `ceil(N / frames-per-window)`
    /// per frame — the 3N /proc reads spread across frames instead of bursting
    /// in one (that burst froze the UI with many tabs), and no shell is ever
    /// read every frame.
    fn sample_shells(&mut self) {
        let total: usize = self.tabs.iter().map(|t| t.shells.len()).sum();
        if total == 0 {
            return;
        }
        let frames = (SAMPLE_EVERY.as_millis() / POLL_INTERVAL.as_millis()).max(1) as usize;
        for _ in 0..total.div_ceil(frames) {
            let (ti, si) = self.flat_index(self.proc_cursor % total);
            self.proc_cursor = (self.proc_cursor + 1) % total;
            if let Some(shell) = self.tabs.get_mut(ti).and_then(|t| t.shells.get_mut(si)) {
                shell.sample_proc();
            }
        }
    }

    /// Translate a flat shell index (0..total) into (tab, shell) coordinates.
    fn flat_index(&self, mut n: usize) -> (usize, usize) {
        for (ti, tab) in self.tabs.iter().enumerate() {
            let len = tab.shells.len();
            if n < len {
                return (ti, n);
            }
            n -= len;
        }
        (0, 0)
    }

    /// The selected screen text, read in reading order with the end cell made
    /// inclusive. `None` when the shell has closed or the selection is blank.
    ///
    /// A finalized selection carries its own snapshot (`sel.text`), so the copy
    /// is exactly the highlight even if the inner app repaints afterwards — the
    /// live screen is only read for a selection still being dragged or one built
    /// directly (tests). A block selection joins each row's columns with a
    /// newline, so the clipboard holds precisely the rectangle that was drawn.
    fn selection_text(&self, sel: Selection) -> Option<String> {
        if let Some(text) = &sel.text {
            return (!text.is_empty()).then(|| text.clone());
        }
        let (t, s) = sel.shell;
        let shell = self.tabs.get(t).and_then(|tab| tab.shells.get(s))?;
        let (a, b) = order(sel.anchor, sel.head);
        let text = if sel.block {
            block_text(shell, a, b, self.pty_cols)
        } else {
            shell.screen_between(a, (b.0, (b.1 + 1).min(self.pty_cols)))
        };
        let text = text.trim_end().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Push the current selection to the clipboard.
    /// Returns whether any (non-blank) text was actually copied.
    fn copy_selection(&self, sel: Selection) -> bool {
        match self.selection_text(sel) {
            Some(text) => {
                copy_clipboard(&text);
                true
            }
            None => false,
        }
    }

    /// Start the footer's "✓ copied" hint when a copy actually carried text.
    fn flash_copy(&mut self, copied: bool) {
        if copied {
            self.copied_at = Some(Instant::now());
        }
    }

    /// Select `span` outright (double/triple click, Alt+a) and copy it. The
    /// selection stays on screen, so what landed on the clipboard is visible.
    fn select_span(&mut self, shell: (usize, usize), (anchor, head): ((u16, u16), (u16, u16))) {
        let sel = Selection { shell, anchor, head, dragging: false, block: false, text: None };
        // Snapshot the text now so the copy is exactly the highlight even if
        // the inner app repaints before the clipboard write lands.
        let text = self.selection_text(sel.clone());
        let sel = Selection { text, ..sel };
        self.selection = Some(sel.clone());
        self.flash_copy(self.copy_selection(sel));
    }

    /// Alt+a: select the whole visible screen and copy it — "copy everything",
    /// but drawn as a selection first, so it is never a surprise what was taken.
    fn select_all(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let shell = (self.active, tab.active);
        let last = (self.pty_rows.saturating_sub(1), self.pty_cols.saturating_sub(1));
        self.select_span(shell, ((0, 0), last));
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
        // Tabs closing before the active one shift it down with them, so focus
        // (and the selection keyed to it) stays on the tab the user was in.
        let dead_before =
            self.tabs[..self.active.min(self.tabs.len())].iter().filter(|t| t.shells.is_empty()).count();
        self.active = self.active.saturating_sub(dead_before);
        self.tabs.retain(|t| !t.shells.is_empty());
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        // `shown` holds indices into `tabs`, which just shrank. A whole burst
        // of events is drained between two frames, so leaving it stale until
        // the next loop turn let a tab-switch right after a close resolve its
        // row against the old order — `navigate_tabs` could then land `active`
        // on a removed index and the next pane click would index out of bounds.
        self.refresh_shown();
    }

    /// Send typed input (keys, pastes) to the active shell.
    fn write_active(&mut self, bytes: &[u8]) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let shell = tab.active_shell_mut();
        // Any input snaps the view back to live output — a selection made on
        // the scrolled view would then highlight unrelated text.
        if shell.scroll(0) != 0 {
            self.selection = None;
        }
        shell.last_input = Instant::now(); // the user is here: a nudge waits
        shell.note_input(bytes); // capture the typed command for `replay`, off the live view
        shell.send(bytes);
    }

    /// Forward an encoded mouse report, which is not typed input: it must not
    /// snap the scrollback, and above all must not go through `note_input`,
    /// whose /proc foreground check would then run on every motion event an
    /// any-motion app subscribes to — hundreds a second under a moving cursor.
    fn forward(&mut self, bytes: &[u8]) {
        if let Some(tab) = self.tabs.get(self.active) {
            tab.active_shell().send(bytes);
        }
    }

    /// Keep every PTY sized to the terminal pane; resize on change.
    /// Degenerate areas (e.g. a 0×0 host PTY) are ignored — vt100 cannot
    /// represent screens that small.
    fn fit_ptys(&mut self, area: Rect) {
        self.term_width = area.width;
        // Re-clamp a dragged-wide sidebar when the terminal itself shrinks,
        // so the pane never collapses to nothing.
        let max = area.width.saturating_sub(MIN_PANE_WIDTH).max(MIN_SIDEBAR_WIDTH);
        self.sidebar_width = self.sidebar_width.clamp(MIN_SIDEBAR_WIDTH, max);
        if area.height < 3 || area.width <= self.sidebar_width + 1 {
            return;
        }
        let rows = area.height - 1; // one line reserved for the status bar
        let cols = area.width - self.sidebar_width;
        if (rows, cols) != (self.pty_rows, self.pty_cols) {
            (self.pty_rows, self.pty_cols) = (rows, cols);
            self.selection = None; // grid reflowed — old cell coords are stale
            for tab in &mut self.tabs {
                tab.resize(rows, cols);
            }
        }
    }

    /// Git branch for `cwd`, cached: re-read when the directory changes or the
    /// 500 ms sample expires — `.git/HEAD` IO stays out of the render path
    /// while branch switches still show up promptly.
    fn git_branch_cached(&mut self, cwd: Option<&Path>) -> Option<String> {
        if self.branch_cwd.as_deref() != cwd || self.branch_sampled.elapsed() >= SAMPLE_EVERY {
            self.branch_sampled = Instant::now();
            self.branch_cwd = cwd.map(Path::to_path_buf);
            self.branch = cwd.and_then(git_branch);
        }
        self.branch.clone()
    }
}

// ── ui ───────────────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [sidebar, pane] =
        Layout::horizontal([Constraint::Length(app.sidebar_width), Constraint::Min(1)]).areas(body);

    // Sidebar: the " ricon " title row, then the search row, then the tabs.
    let block = Block::new().borders(Borders::RIGHT).title(Line::from(" ricon ").bold().centered());
    let inner = block.inner(sidebar);
    frame.render_widget(block, sidebar);
    let [search_area, list_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    // Search row (kata ui.md): filters the tab list as it is typed; a block
    // caret marks focus (app start, Alt+f, or a click on the row).
    let caret = if app.search_focus { "█" } else { "" };
    let search_style = if app.search_focus { Style::new().bold() } else { Style::new().fg(Color::DarkGray) };
    frame.render_widget(Line::styled(format!(" ⌕ {}{caret}", app.search), search_style), search_area);
    // `list_offset` is the authoritative scroll position: the wheel moves it
    // and a changed active tab is revealed into it (see `reveal_active`), so
    // the render only honours it — nothing may yank the view back to the
    // active tab and fight free scrolling. Clamp first as the tab count,
    // filter or sidebar height may have shrunk since the last scroll; then
    // build items for the visible (shown) tabs only.
    app.refresh_shown();
    app.sidebar_rows = sidebar.height;
    app.list_offset = app.list_offset.min(app.max_offset());
    let visible = app.visible_tabs();
    let items =
        app.shown[visible].iter().map(|&i| tab_item(i, &app.tabs[i], i == app.active, app.sidebar_width));
    frame.render_widget(List::new(items), list_area);

    if app.active < app.tabs.len() {
        let cwd = app.tabs[app.active].active_shell().cwd.clone();
        let branch = app.git_branch_cached(cwd.as_deref());
        let tab = &app.tabs[app.active];
        let shell = tab.active_shell();
        let parser = shell.parser.lock().unwrap_or_else(PoisonError::into_inner);
        frame.render_widget(PseudoTerminal::new(parser.screen()), pane);
        // Overlay the live selection in reading order. The cells are painted
        // outright rather than reversed: reversing cancels itself out over text
        // that is already inverse (prompts, status lines, a selected menu row),
        // which left holes in the highlight exactly where it mattered.
        if let Some(sel) = app.selection.as_ref().filter(|s| s.shell == (app.active, tab.active)) {
            let (a, b) = order(sel.anchor, sel.head);
            let buf = frame.buffer_mut();
            let cells: Box<dyn Iterator<Item = (u16, u16)>> = if sel.block {
                Box::new(block_cells(a, b))
            } else {
                Box::new(selection_cells(a, b, app.pty_cols))
            };
            for (r, c) in cells {
                if r < pane.height
                    && c < pane.width
                    && let Some(cell) = buf.cell_mut((pane.x + c, pane.y + r))
                {
                    cell.set_bg(SELECT_BG);
                    cell.set_fg(SELECT_FG);
                    cell.modifier.remove(Modifier::REVERSED);
                }
            }
        }
        frame.render_widget(
            status_bar(Footer {
                index: app.active,
                count: app.tabs.len(),
                shell,
                branch,
                width: footer.width,
                tabs_overflow: app.tabs_overflow(),
                copy_mode: app.copy_mode,
                auto: tab.auto,
            }),
            footer,
        );
    }
    // Transient copy confirmation: a small "✓ copied" pinned to the footer for
    // ~COPY_HINT after a copy, so the user sees it reached the clipboard. It
    // lands just left of the copy button (never over it — the button has to
    // stay clickable and legible right when it was used), and only briefly.
    if app.copied_at.is_some_and(|t| t.elapsed() < COPY_HINT) {
        const HINT: &str = " ✓ copied ";
        // Right edge of the hint: the copy button's first column, or the
        // footer's own edge when it is too narrow to carry the button.
        let edge = copy_button_x(footer.width).map_or(footer.width, |(from, _)| from);
        let w = (HINT.chars().count() as u16).min(edge);
        let rect = Rect { x: footer.x + edge - w, y: footer.y, width: w, height: 1 };
        frame.render_widget(
            Line::from(HINT).style(Style::new().bg(Color::Green).fg(Color::Black).bold()),
            rect,
        );
    }
    if let Some(yes) = app.confirm_quit {
        draw_quit_confirm(frame, yes);
    }
    if app.help {
        draw_help(frame);
    }
}

/// Every shortcut of ricon's own, for the cheat sheet (kata app.md).
const SHORTCUTS: &[(&str, &str)] = &[
    ("Alt+t / Alt+n", "new tab"),
    ("Alt+s", "new subshell in the tab"),
    ("Alt+w", "close the active shell"),
    ("Alt+↑ / Alt+↓", "previous / next shell"),
    ("Alt+PgUp / PgDn", "previous / next tab"),
    ("Alt+1 … Alt+9", "tab by number"),
    ("Alt+f", "search tabs"),
    ("Alt+Shift+f", "favorite (pinned on top)"),
    ("Alt+r", "replay the last command"),
    ("Alt+c", "copy mode on / off"),
    ("Alt+a", "select the screen + copy"),
    ("Alt+?", "this cheat sheet"),
    ("Alt+q", "quit"),
    ("drag · 2× · 3× click", "select + copy"),
    ("middle click", "paste"),
    ("Alt+drag", "select with copy mode off"),
];

/// Centered cheat sheet listing every shortcut; any key or click closes it.
/// Drawn last, over everything.
fn draw_help(frame: &mut Frame) {
    let key_w = SHORTCUTS.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let rows: Vec<String> = SHORTCUTS.iter().map(|(k, what)| format!(" {k:<key_w$}   {what} ")).collect();
    let area = frame.area();
    let width = (rows.iter().map(|r| r.chars().count()).max().unwrap_or(0) as u16 + 2).min(area.width);
    let height = (rows.len() as u16 + 2).min(area.height);
    let popup =
        Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 2, width, height };
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(Line::from(" shortcuts ").bold().centered());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines: Vec<Line> = rows
        .iter()
        .map(|r| {
            let (key, what) = r.split_at(r.char_indices().nth(key_w + 1).map_or(r.len(), |(i, _)| i));
            Line::from(vec![Span::styled(key.to_string(), Style::new().bold()), Span::raw(what.to_string())])
        })
        .collect();
    frame.render_widget(List::new(lines.into_iter().map(ListItem::new)), inner);
}

/// Centered modal asking "Are you sure to Quit all tabs?" with YES / NO
/// buttons; the highlighted one is rendered reversed. Drawn last so it sits
/// on top of the sidebar and pane.
fn draw_quit_confirm(frame: &mut Frame, yes: bool) {
    const QUESTION: &str = "Are you sure to Quit all tabs?";
    let area = frame.area();
    let width = (QUESTION.len() as u16 + 4).min(area.width);
    let height = 5.min(area.height);
    let popup =
        Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 2, width, height };
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(Line::from(" Quit ").bold().centered());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [question, _, buttons] = Layout::vertical([Constraint::Length(1); 3]).areas(inner);
    frame.render_widget(Line::from(QUESTION).centered(), question);
    let button = |label, selected: bool| {
        let style = if selected { Style::new().bold().reversed() } else { Style::new().fg(Color::DarkGray) };
        Span::styled(label, style)
    };
    frame.render_widget(
        Line::from(vec![button("  YES  ", yes), Span::raw("   "), button("  NO  ", !yes)]).centered(),
        buttons,
    );
}

/// Everything the status bar draws, in one place.
struct Footer<'a> {
    index: usize,
    count: usize,
    shell: &'a Shell,
    branch: Option<String>,
    width: u16,
    tabs_overflow: bool,
    copy_mode: bool,
    /// The tab's auto feature is on: the footer then counts down to the nudge.
    auto: bool,
}

/// Footer: an up-down arrow when the tab list overflows, then the active
/// terminal number / tab count, its location, git branch and the agent's
/// model with its context usage on the left, the copy-mode button and the app
/// version pinned to the right corner.
fn status_bar(f: Footer) -> Line<'static> {
    let Footer { index, count, shell, branch, width, tabs_overflow, copy_mode, auto } = f;
    let path = shell.cwd.as_deref().map_or_else(|| "?".into(), |p| abbreviate_home(&p.display().to_string()));
    let branch = branch.map_or_else(String::new, |b| format!("  ⎇ {b}"));
    // The agent's model and how full its context is, then what the auto
    // feature is up to: the countdown to its next nudge, or its glyph while a
    // nudge is still unanswered — it types into the agent on the user's
    // behalf, so it must never do so invisibly. The mark clears on the agent's
    // next output, which is exactly when `nudge` is re-armed.
    let context = shell.agent.as_ref().map_or_else(String::new, |a| context_label(a.context));
    let auto_state = match shell.nudge {
        Some(Nudge::Compacting(_)) => format!(" ⟳ {COMPACT_COMMAND}"),
        Some(Nudge::Continued(_)) => " ⟳".to_string(),
        None => match shell.countdown().filter(|_| auto) {
            Some(left) => format!(" ⏳ {} → {COMPACT_COMMAND}", clock(left)),
            None => String::new(),
        },
    };
    let agent =
        shell.agent.as_ref().map_or_else(String::new, |a| format!("  ✳ {}{context}{auto_state}", a.model));
    let right = format!("v{} ", env!("CARGO_PKG_VERSION"));
    // The button is dropped entirely on a footer too narrow to hold it —
    // `copy_button_x` decides that once, for both this render and the hit-test.
    let label = if copy_mode { COPY_ON_BUTTON } else { COPY_OFF_BUTTON };
    let button = if copy_button_x(width).is_some() { label } else { "" };
    // Scroll indicator sits before the active tab index when not all tabs fit.
    let scroll = if tabs_overflow { "↕ " } else { "" };
    // Left segment, truncated so the button and the right-corner version fit.
    let mut left = format!(" {scroll}{}/{count} ▸ {path}{branch}{agent}", index + 1);
    // Measured in cells, not chars: the countdown's ⏳ (or a wide path or
    // model name) takes two, and counting it as one shifted the button a cell
    // right of where `copy_button_x` hit-tests it and cut the version short.
    let fixed = right.width() + button.width();
    let room = (width as usize).saturating_sub(fixed);
    if left.width() > room {
        let mut used = 0;
        left = left
            .chars()
            .take_while(|c| {
                used += c.width().unwrap_or(0);
                used <= room
            })
            .collect();
    }
    let pad = (width as usize).saturating_sub(left.width() + fixed);
    let style = Style::new().bg(ACTIVE_BG).fg(ACTIVE_FG).bold();
    // The usage turns red once the context is nearly full (kata ai.md): the
    // label is split out of the left segment where it survived truncation.
    let warn = shell
        .agent
        .as_ref()
        .and_then(|a| a.context)
        .and_then(Context::fill)
        .is_some_and(|f| f >= CONTEXT_WARN_AT);
    let mut spans = match left.find(&context).filter(|_| warn && !context.is_empty()) {
        Some(at) => vec![
            Span::raw(left[..at].to_string()),
            Span::styled(context.clone(), style.fg(CONTEXT_WARN_FG)),
            Span::raw(format!("{}{}", &left[at + context.len()..], " ".repeat(pad))),
        ],
        None => vec![Span::raw(format!("{left}{}", " ".repeat(pad)))],
    };
    // The button carries its state: the selection color while copy mode is
    // on, a plain reversed button while it is off.
    let button_style = if copy_mode { style.bg(SELECT_BG).fg(SELECT_FG) } else { style.reversed() };
    spans.push(Span::styled(button, button_style));
    spans.push(Span::raw(right));
    Line::from(spans).style(style)
}

/// The red a nearly full context is painted in — deep enough to read on the
/// pastel.
const CONTEXT_WARN_FG: Color = Color::Rgb(178, 24, 24);

/// `m:ss` for the footer countdown.
fn clock(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// ` 120k/1M` — an agent's context usage for the footer, or nothing when it
/// is unknown. The window is left out when only the usage could be read.
fn context_label(context: Option<Context>) -> String {
    match context {
        Some(Context { used, max: Some(max) }) => format!(" {}/{}", tokens(used), tokens(max)),
        Some(Context { used, max: None }) => format!(" {}", tokens(used)),
        None => String::new(),
    }
}

/// A token count at footer size: `999`, `12k`, `1M`, `1.5M`.
fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_499 => format!("{}k", (n + 500) / 1_000),
        _ => {
            let tenths = (n + 50_000) / 100_000;
            if tenths.is_multiple_of(10) {
                format!("{}M", tenths / 10)
            } else {
                format!("{}.{}M", tenths / 10, tenths % 10)
            }
        }
    }
}

/// Footer columns `[from, to)` holding the copy-mode button — flush left of
/// the version corner. `None` when the footer is too narrow to carry both, in
/// which case the button is neither drawn nor clickable.
fn copy_button_x(width: u16) -> Option<(u16, u16)> {
    let version = concat!("v", env!("CARGO_PKG_VERSION"), " ").chars().count() as u16;
    let to = width.checked_sub(version)?;
    let from = to.checked_sub(BUTTON_COLS)?;
    (from > 0).then_some((from, to))
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

/// Process group of `pid` (field 5 of /proc/<pid>/stat).
fn proc_pgrp(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 2..].split_whitespace().nth(2)?.parse().ok()
}

fn proc_comm(pid: u32) -> Option<String> {
    Some(std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?.trim().to_string())
}

/// Full command line of process `pid`, NUL-separated args joined with spaces.
fn proc_cmdline(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let cmd = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    (!cmd.is_empty()).then_some(cmd)
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
        Source::SessionTail(dir) => session_tail_model(dir, pid),
        Source::OpencodeSelected => opencode_selected(pid),
    }
}

/// Most recently modified `*.<ext>` file directly under `dir`.
fn newest_file(dir: &Path, ext: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == ext))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// Model of the session transcript agent `pid` is writing (see `session_file`).
/// Every assistant line in that JSONL carries the model that answered, so the
/// last one is the model in force right now, `/model` switches included. Only
/// the file's tail is read, and the whole probe runs off the render thread
/// (see `AgentProbe`).
fn session_tail_model(dir: &str, pid: u32) -> Option<String> {
    scan_tail(&session_file(dir, pid)?, last_session_model)
}

/// Context usage of that same session: what its last answer was billed for as
/// input, and the window it sits in.
fn session_context(dir: &str, pid: u32) -> Option<Context> {
    let used = scan_tail(&session_file(dir, pid)?, last_session_usage)?;
    // The window is the model's, and Claude Code names the wide one with a
    // `[1m]` suffix on the model it was configured with — a suffix the
    // transcript strips. A usage past the narrow window is proof of the wide
    // one whatever the configuration says.
    let base = Path::new(dir).parent().and_then(Path::to_str).unwrap_or_default();
    let configured = env_var(pid, "ANTHROPIC_MODEL")
        .or_else(|| settings_value(&format!("{base}/settings.json"), "model"))
        .unwrap_or_default();
    // A `--model …[1m]` on the command line outranks both.
    let launched = proc_cmdline(pid).is_some_and(|cmd| cmd.contains("[1m]"));
    let wide = launched || configured.contains("[1m]") || used > CONTEXT_WINDOW;
    Some(Context { used, max: Some(if wide { WIDE_CONTEXT_WINDOW } else { CONTEXT_WINDOW }) })
}

/// Claude Code's context windows: the default, and the one a `[1m]` model
/// carries.
const CONTEXT_WINDOW: u64 = 200_000;
const WIDE_CONTEXT_WINDOW: u64 = 1_000_000;

/// `find` over the tail of `path`, widened until it answers. Transcript lines
/// are unbounded — one tool result can be a few hundred KiB — so a fixed tail
/// can hold nothing but the line written after the answer being looked for.
/// The line the tail starts inside is dropped: it is not whole, and a cut
/// line can lose the very marker (`isSidechain`) that disqualifies it.
fn scan_tail<T>(path: &Path, find: impl Fn(&str) -> Option<T>) -> Option<T> {
    const TAILS: [u64; 3] = [64 * 1024, 1024 * 1024, 8 * 1024 * 1024];
    let len = std::fs::metadata(path).ok()?.len();
    TAILS.iter().find_map(|&max| {
        let bytes = read_tail(path, max)?;
        let text = String::from_utf8_lossy(&bytes);
        let whole = if len > max { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
        // Past the whole file, a wider read would find nothing new: stop.
        find(whole).map(Some).or_else(|| (len <= max).then_some(None))
    })?
}

/// The session transcript agent `pid` is writing, under `<$HOME>/<dir>/<slug>`
/// where `slug` is the agent process's own working directory with every
/// non-alphanumeric character replaced by `-` — how Claude Code and its forks
/// name a project's transcript folder. Which file in that folder is *this*
/// session's: the client registers itself under `<$HOME>/<base>/sessions/<pid>.json`
/// with its session id, and the transcript is named by that id — so two
/// sessions open in the same project each resolve to their own. A client that
/// registers nothing (an older version, a fork) falls back to the newest
/// transcript in the folder.
fn session_file(dir: &str, pid: u32) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let slug: String =
        cwd.to_str()?.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let project = PathBuf::from(format!("{home}/{dir}/{slug}"));
    let base = Path::new(dir).parent().and_then(Path::to_str).unwrap_or_default();
    let registered = std::fs::read_to_string(format!("{home}/{base}/sessions/{pid}.json"))
        .ok()
        .and_then(|json| json_string(&json, "sessionId"));
    // A registered session is the only answer, even before its transcript
    // exists (a new session, or just after `/clear`): the newest file then
    // belongs to another session, and its model and usage are not this one's.
    match registered {
        Some(id) => Some(project.join(format!("{id}.jsonl"))).filter(|file| file.is_file()),
        None => newest_file(&project, "jsonl"),
    }
}

/// The model of the last main-session answer in a transcript tail: one JSON
/// object per line, the model in a `"model":"…"` field. Subagent turns land in
/// the same file with their own (often smaller) model and are marked
/// `"isSidechain":true` — the tab is running the main session, so they are
/// skipped, as are the `<…>` placeholders written for messages no model
/// produced. Kept a plain scan: this runs on the probe thread twice a second.
fn last_session_model(text: &str) -> Option<String> {
    text.lines()
        .filter(|line| !line.contains("\"isSidechain\":true"))
        .filter_map(|line| {
            line.match_indices("\"model\":\"")
                .filter_map(|(at, m)| {
                    let rest = &line[at + m.len()..];
                    let value = &rest[..rest.find('"')?];
                    (!value.is_empty() && !value.starts_with('<')).then(|| value.to_string())
                })
                .last()
        })
        .next_back()
}

/// The context the last main-session answer in a transcript tail was billed
/// for: its `usage` — prompt tokens, plus the cache written and read, which is
/// the rest of the conversation. The first of each field after `"usage":{` is
/// the answer's own; the per-iteration copies further along are skipped.
/// Sidechain (subagent) answers are skipped like in `last_session_model`.
fn last_session_usage(text: &str) -> Option<u64> {
    let usage = text
        .lines()
        .rev()
        .filter(|line| !line.contains("\"isSidechain\":true"))
        .find_map(|line| line.split_once("\"usage\":{").map(|(_, rest)| rest))?;
    let field = |key: &str| json_number(usage, key).unwrap_or(0);
    Some(field("input_tokens") + field("cache_creation_input_tokens") + field("cache_read_input_tokens"))
}

/// First integer value for `"key":` in a JSON blob (naive, like `json_string`).
fn json_number(text: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let rest = text[text.find(&needle)? + needle.len()..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Resolve an agent's context usage from where its client keeps it.
fn resolve_usage(usage: &Usage, pid: u32) -> Option<Context> {
    match usage {
        Usage::ClaudeSession(dir) => session_context(dir, pid),
        Usage::Opencode => opencode_context(pid),
    }
}

/// The agent's own status, where its client reports one: Claude Code keeps
/// `status` (`idle`, `busy`, `shell`…) and `statusUpdatedAt` (ms since the
/// epoch) in its registration under `sessions/<pid>.json`. Anything but
/// `idle` is busy; opencode reports nothing, and the screen hash stands in.
fn resolve_status(usage: &Usage, pid: u32) -> Option<Status> {
    let Usage::ClaudeSession(dir) = usage else { return None };
    let home = std::env::var("HOME").ok()?;
    let base = Path::new(dir).parent().and_then(Path::to_str).unwrap_or_default();
    let json = std::fs::read_to_string(format!("{home}/{base}/sessions/{pid}.json")).ok()?;
    session_status(&json)
}

/// `status`/`statusUpdatedAt` out of a Claude Code session registration.
fn session_status(json: &str) -> Option<Status> {
    let status = json_string(json, "status")?;
    Some(match (status.as_str(), json_number(json, "statusUpdatedAt")) {
        ("idle", Some(ms)) => Status::Idle(UNIX_EPOCH + Duration::from_millis(ms)),
        ("idle", None) => Status::Idle(SystemTime::now()),
        _ => Status::Busy,
    })
}

/// Last `key=value` token in the most recently modified `*.log` under the
/// `$HOME`-relative `dir`; only the file's tail is read to bound the cost.
fn log_tail_value(dir: &str, key: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let newest = newest_file(Path::new(&format!("{home}/{dir}")), "log")?;
    let buf = read_tail(&newest, 64 * 1024)?;
    let text = String::from_utf8_lossy(&buf);
    let needle = format!("{key}=");
    let rest = &text[text.rfind(&needle)? + needle.len()..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let val = rest[..end].trim_matches('"');
    (!val.is_empty()).then(|| val.to_string())
}

/// opencode's currently selected model for the agent process's project dir
/// (see `opencode_session`).
fn opencode_selected(pid: u32) -> Option<String> {
    json_string(&opencode_session(pid)?.1, "id")
}

/// opencode's context usage for the agent's session: the token total of the
/// last answer in it (the conversation as the model saw it), against the
/// model's context limit from opencode's own model catalog cache — left out
/// when the catalog has no entry for it.
fn opencode_context(pid: u32) -> Option<Context> {
    use rusqlite::{Connection, OpenFlags};
    let (session, model) = opencode_session(pid)?;
    let conn = Connection::open_with_flags(opencode_db()?, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(100));
    // The last *finished* answer: the one still streaming has no total yet
    // (0), and reading it made the footer flicker to nothing on every probe.
    let used: u64 = conn
        .query_row(
            "SELECT json_extract(data, '$.tokens.total') FROM message \
             WHERE session_id = ?1 AND json_extract(data, '$.role') = 'assistant' \
               AND json_extract(data, '$.tokens.total') > 0 \
             ORDER BY time_updated DESC LIMIT 1",
            [session],
            |row| row.get(0),
        )
        .ok()?;
    let max = json_string(&model, "providerID")
        .zip(json_string(&model, "id"))
        .and_then(|(provider, id)| opencode_context_limit(&provider, &id));
    Some(Context { used, max })
}

/// The context limit opencode's model catalog gives `provider`'s model `id`.
/// The catalog nests models under their provider, and the same model id is
/// listed again under every aggregator reselling it — often with another
/// limit — so the entry is addressed by path, never searched for. It is a
/// multi-MiB file, so the answer is kept until the file changes.
fn opencode_context_limit(provider: &str, id: &str) -> Option<u64> {
    type Cached = (SystemTime, String, Option<u64>);
    static CACHE: Mutex<Option<Cached>> = Mutex::new(None);
    if provider.contains('"') || id.contains('"') {
        return None; // not addressable in a JSON path
    }
    let path = PathBuf::from(std::env::var("HOME").ok()?).join(".cache/opencode/models.json");
    let modified = std::fs::metadata(&path).and_then(|meta| meta.modified()).ok()?;
    let key = format!("$.\"{provider}\".models.\"{id}\".limit.context");
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((at, cached, limit)) = cache.as_ref()
        && *at == modified
        && *cached == key
    {
        return *limit;
    }
    let catalog = std::fs::read_to_string(&path).ok()?;
    let limit = rusqlite::Connection::open_in_memory()
        .and_then(|db| db.query_row("SELECT json_extract(?1, ?2)", [&catalog, &key], |row| row.get(0)))
        .ok()
        .flatten();
    *cache = Some((modified, key, limit));
    limit
}

/// opencode's most recently used session for the agent process's project
/// dir: its id and its model (a JSON blob with the provider and model ids).
/// opencode is event-sourced into a SQLite DB; the live selection is the model
/// of the most recently updated `session` row for that directory. Reading it
/// via SQLite (vs. scanning bytes) is essential — WAL frame ordering makes raw
/// scans return stale models. Opened read-only so opencode is never disturbed.
fn opencode_session(pid: u32) -> Option<(String, String)> {
    use rusqlite::{Connection, OpenFlags};
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let dir = cwd.to_str()?;
    let conn = Connection::open_with_flags(opencode_db()?, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(100));
    conn.query_row(
        "SELECT id, model FROM session \
         WHERE directory = ?1 AND model IS NOT NULL \
         ORDER BY time_updated DESC LIMIT 1",
        [dir],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .ok()
}

fn opencode_db() -> Option<String> {
    Some(format!("{}/.local/share/opencode/opencode.db", std::env::var("HOME").ok()?))
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
    // The first occurrence that is a key (followed by `:`) holding a string:
    // `"model": null` is no model and must not borrow the next key's value,
    // and a `"model"` that is itself a value is no key at all.
    let (at, _) = text
        .match_indices(&needle)
        .find(|(at, _)| text[at + needle.len()..].trim_start().starts_with(':'))?;
    let rest = text[at + needle.len()..].trim_start()[1..].trim_start();
    let value = rest.strip_prefix('"')?;
    Some(value[..value.find('"')?].to_string()).filter(|v| !v.is_empty())
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
/// `*` after it and the tab's `auto` button glued to the panel's right edge —
/// kata ai.md), full location path,
/// running process with the activity spinner appended
/// while output is streaming (+1 s after it settles), and a fourth row that is
/// empty on an inactive tab and carries the activity bar on the active one.
/// Each subshell then adds two rows — its path and running process — shown bold
/// while it is the tab's active shell. Content lines of the active tab carry a
/// `│` as their first character (the last row included). Only the active tab is
/// painted — the pastel `ACTIVE_BG` under dark text; every other tab draws on
/// the terminal's own colors (kata app.md).
fn tab_item(index: usize, tab: &Tab, is_active: bool, width: u16) -> ListItem<'static> {
    let style = if is_active { Style::new().bg(ACTIVE_BG).fg(ACTIVE_FG) } else { Style::new() };
    // The spinner is white (kata app.md) — except on the pastel, where white
    // would vanish: there it takes the tab's dark text color.
    let spin_style = style.fg(if is_active { ACTIVE_FG } else { SPINNER_COLOR }).bold();
    let parent = &tab.shells[0];
    let cwd = parent.cwd.as_deref();
    let marker = if is_active { "▶" } else { " " };
    // Every content line of the active tab starts with a `│` gutter column;
    // inactive tabs get a space so columns stay aligned across the list.
    let bar = if is_active { "│" } else { " " };
    // Any shell producing output while off screen flags the whole tab.
    let unseen = tab.shells.iter().any(|s| s.unseen_output);
    let folder = cwd.map_or_else(|| "?".into(), folder_name);
    let full = cwd.map_or_else(|| "?".into(), |p| abbreviate_home(&p.display().to_string()));
    // A ⭐ sits before the folder name on favorites (a wide glyph, so it steals
    // three columns from the name's width budget). The auto button is glued to
    // the panel's right edge, so the name gets the columns left between the
    // prefix (gutter, marker, number, star) and that button, minus the unseen
    // `*` and its own leading space.
    let button = auto_button(index, tab, width);
    let margins = tab_margins(index, tab);
    // Columns the name may occupy, its own leading space included: everything
    // between the row's margins and the button's fixed column, or the rest of
    // the panel when this row carries no button. Spending the space out of that
    // budget rather than beside it is what keeps the row from growing past
    // `button.start` on a narrow panel — a name that pushed the button one
    // column right would leave the hit-test pointing where the button was.
    let limit = button.as_ref().map_or(width.saturating_sub(1) as usize, |b| b.start);
    let budget = limit.saturating_sub(margins);
    let name = match budget {
        0 => String::new(),
        _ => format!(" {}", truncate_tail(&folder, budget as u16, 1)),
    };
    // Blanks between the last indicator and the button's fixed column.
    let gap = " ".repeat(limit.saturating_sub(margins + name.chars().count()));
    // The active shell within a multi-shell tab is flagged with ▶ on its path
    // row, indented two spaces so it nests under the tab-level ▶ (rule: "active
    // shell shows ▶ … with two prefix spaces"); single-shell tabs rely on the
    // tab marker alone, so the active-tab ▶ isn't doubled on every tab.
    let multishell = tab.shells.len() > 1;
    let shell_mark = |active: bool| if multishell && active { "▶" } else { " " };
    let path = format!("{bar}  {} {}", shell_mark(tab.active == 0), truncate_tail(&full, width, 6));
    let top = if is_active { style.bold() } else { style };
    let mut first = vec![Span::styled(format!("{bar}{marker}{}", index + 1), top)];
    if tab.favorite {
        first.push(Span::styled(" ⭐", style.fg(Color::Yellow).bold()));
    }
    first.push(Span::styled(name, top));
    // Off-screen output flags the tab with a bold `*` after its name.
    if unseen {
        first.push(Span::styled(" *", style.bold()));
    }
    // The tab's `auto` button ends the first line, after the text and its
    // indicators and glued to the panel's right edge (kata ai.md) — the buttons
    // then form one column down the sidebar instead of jittering with every
    // folder name. Its background carries the state: green when the tab's
    // continue feature is on, gray when off; its label turns yellow while a
    // nudge typed into this tab is still unanswered. Only the colors ever
    // change — the label keeps its `AUTO_COLS` width and its column, so
    // `auto_button`, which the hit-test reads too, stays in sync. A panel too
    // narrow to seat it at that column draws no button at all, and then the
    // hit-test finds none either.
    if button.is_some() {
        first.push(Span::styled(gap, top));
        let nudged = tab.shells.iter().any(|s| s.nudge.is_some());
        let (bg, fg) = match (tab.auto, nudged) {
            (true, false) => (AUTO_ON_BG, AUTO_ON_FG),
            (true, true) => (AUTO_ON_BG, AUTO_NUDGED_FG),
            (false, _) => (AUTO_OFF_BG, AUTO_OFF_FG),
        };
        first.push(Span::styled(AUTO_BUTTON, Style::new().bg(bg).fg(fg)));
    }
    // Parent rows: name, path, process (+ spinner). The blank separator is
    // pushed last (after any subshells) so it always divides this tab's last
    // row from the next tab, not the parent from its own subshells.
    // Parent shell info is bold when the parent is the active shell (multishell
    // only; single-shell tabs use the tab marker alone, matching `shell_mark`).
    let parent_style = if multishell && tab.active == 0 { style.bold() } else { style };
    let mut lines = vec![
        Line::from(first),
        Line::styled(path, parent_style),
        Line::from(process_row(parent, bar, parent_style, spin_style)),
    ];
    // A replay row follows the process row whenever that shell is replayable.
    lines.extend(replay_row(parent, bar, width, parent_style));
    // Path + process rows per subshell (bold while active), each trailed by its
    // own replay row when replayable.
    for (si, sub) in tab.shells.iter().enumerate().skip(1) {
        let s = if tab.active == si { style.bold() } else { style };
        let sfull =
            sub.cwd.as_deref().map_or_else(|| "?".into(), |p| abbreviate_home(&p.display().to_string()));
        lines.push(Line::styled(
            format!("{bar}  {} {}", shell_mark(tab.active == si), truncate_tail(&sfull, width, 6)),
            s,
        ));
        lines.push(Line::from(process_row(sub, bar, s, spin_style)));
        lines.extend(replay_row(sub, bar, width, s));
    }
    // Last row separating this tab from the next: empty on an inactive tab; on
    // the active one the `│` gutter like every other line, then the activity bar.
    lines.push(activity_row(tab, bar, width, is_active, style));
    ListItem::new(lines).style(style)
}

/// The active tab's last row (kata app.md): a track across the panel with a
/// lit segment sweeping left to right along it while any of the tab's shells
/// is streaming output (the same window the spinner uses), and the track alone
/// while the tab is quiet. An inactive tab keeps the row empty. The sweep is
/// clocked off the parent shell's age, so it runs smoothly across frames
/// without any state of its own.
fn activity_row(tab: &Tab, bar: &str, width: u16, is_active: bool, style: Style) -> Line<'static> {
    // The gutter and the panel's border column are not the bar's.
    let cells = width.saturating_sub(2) as usize;
    if !is_active || cells == 0 {
        return Line::styled(bar.to_string(), style);
    }
    let busy = tab.shells.iter().any(|s| s.animating);
    let mut spans = vec![Span::styled(bar.to_string(), style)];
    let track = |n: usize| Span::styled("─".repeat(n), style.fg(BAR_TRACK));
    match busy.then(|| sweep(tab.shells[0].spawned.elapsed(), cells)) {
        Some((from, to)) => {
            spans.push(track(from));
            spans.push(Span::styled("━".repeat(to - from), style.fg(BAR_LIT).bold()));
            spans.push(track(cells - to));
        }
        None => spans.push(track(cells)),
    }
    Line::from(spans)
}

/// Where the lit segment of an activity bar `cells` wide sits at `elapsed`:
/// the cell range `[from, to)` it covers. The segment enters from the left,
/// leaves to the right and comes round again; `BAR_LEN` cells long, moving one
/// cell per `BAR_STEP`, and never wholly off the bar — a busy tab always shows
/// at least one lit cell.
fn sweep(elapsed: Duration, cells: usize) -> (usize, usize) {
    let head = 1 + (elapsed.as_millis() / BAR_STEP.as_millis()) as usize % (cells + BAR_LEN - 1);
    (head.saturating_sub(BAR_LEN).min(cells), head.min(cells))
}

/// A `└ process` row for one shell, with the braille activity spinner appended
/// while its output is streaming (+1 s after it settles). `bar` is the active-tab
/// gutter column (`|` when the tab is active, a space otherwise).
fn process_row(shell: &Shell, bar: &str, style: Style, spin_style: Style) -> Vec<Span<'static>> {
    // Braille spinner at 0.5 rps: one rotation per 2 s (10 frames × 200 ms).
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let mut row = vec![Span::styled(format!("{bar}   └ {}", shell.process), style)];
    if shell.animating {
        let frame = (shell.spawned.elapsed().as_millis() / 200) as usize % FRAMES.len();
        row.push(Span::styled(format!("  {}", FRAMES[frame]), spin_style));
    }
    row
}

/// The `🔁 <command>` row shown directly below a shell's process row while a
/// shell is the foreground process and has a command to replay (kata tab): the
/// replay icon, then the full command that a click anywhere from the icon to the
/// command's last column — or Alt+r — re-runs. `None` when the shell isn't
/// replayable. The icon starts at `REPLAY_ICON_COL`, one space past the process
/// row's `└`; the row's layout is kept in sync with `App::replay_at`.
fn replay_row(shell: &Shell, bar: &str, width: u16, style: Style) -> Option<Line<'static>> {
    if !shell.replayable() {
        return None;
    }
    let cmd = shell.last_cmd.as_deref().unwrap_or_default();
    Some(Line::from(vec![
        Span::styled(format!("{bar}    {REPLAY_LABEL} "), style.patch(Style::new().fg(REPLAY_COLOR)).bold()),
        Span::styled(truncate_head(cmd, width, REPLAY_PAD + 1), style),
    ]))
}

/// Clickable columns of a shell's replay row: the icon through the last column
/// of the command text `replay_row` draws beside it (never narrower than the
/// icon, so an empty command still leaves a button). Kept in sync with the
/// layout in `replay_row`.
fn replay_span(shell: &Shell, width: u16) -> std::ops::Range<usize> {
    let cmd = shell.last_cmd.as_deref().unwrap_or_default();
    let end = REPLAY_PAD as usize + truncate_head(cmd, width, REPLAY_PAD + 1).chars().count();
    REPLAY_ICON_COL..end.max(REPLAY_ICON_COL + REPLAY_COLS)
}

/// Clickable columns of a tab's `auto` button: on the tab's first (name) row,
/// after its text and indicators and glued to the right edge of the tab panel,
/// just inside its border column (kata ai.md) — one fixed column for every tab,
/// whatever its name, star or unseen marker. Kept in sync with `tab_item`.
fn auto_span(width: u16) -> std::ops::Range<usize> {
    // The border column is the panel's last one and is never the button's:
    // claiming it made the sidebar undraggable on every tab-name row once the
    // panel was narrow enough for the two to meet.
    let end = width.saturating_sub(1) as usize;
    end.saturating_sub(AUTO_COLS as usize)..end
}

/// Columns tab `index` spends on its first line before the folder name: the
/// gutter and active marker, the tab number, and a favourite's ⭐ (a wide
/// glyph), plus the unseen-output `*` that trails the name. This is what
/// competes with the button for the row, so `tab_item` and the hit-test both
/// measure it here rather than each keeping its own copy.
fn tab_margins(index: usize, tab: &Tab) -> usize {
    let star = if tab.favorite { 3 } else { 0 };
    let unseen = if tab.shells.iter().any(|s| s.unseen_output) { 2 } else { 0 };
    2 + (index + 1).to_string().len() + star + unseen
}

/// Where tab `index` draws its `auto` button, or `None` on a panel too narrow
/// to seat it at the fixed column `auto_span` reports — the row's own margins
/// reach that column first, and a button drawn short of it would answer clicks
/// nowhere near where it sits. Dropping it instead keeps the one invariant that
/// matters: what is drawn is exactly what is clickable, at every width.
fn auto_button(index: usize, tab: &Tab, width: u16) -> Option<std::ops::Range<usize>> {
    let span = auto_span(width);
    (!span.is_empty() && span.start >= tab_margins(index, tab)).then_some(span)
}

/// Final path component (the current folder); root-style paths show as-is.
fn folder_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Tail-truncate `s` to the sidebar width, leaving `pad` columns for the
/// border, marker and indent; an elided head is marked with `…`. The result
/// never exceeds the budget — the `…` costs one of the kept columns, so a row
/// that reserves exactly this much can't spill into what follows it (the auto
/// button, the panel border).
fn truncate_tail(s: &str, width: u16, pad: u16) -> String {
    let count = s.chars().count();
    let max = width.saturating_sub(pad) as usize;
    match max {
        // No columns left at all: the `…` would itself be the overflow, and a
        // caller that budgeted nothing must get nothing — a row laid out from
        // these widths puts what follows straight after.
        0 => String::new(),
        _ if count > max => std::iter::once('…').chain(s.chars().skip(count + 1 - max)).collect(),
        _ => s.to_string(),
    }
}

/// Head-truncate `s` to the sidebar width, leaving `pad` columns for the prefix;
/// an elided tail is marked with a trailing `…`. Commands read from the front,
/// so — unlike a path — the head is what's kept.
fn truncate_head(s: &str, width: u16, pad: u16) -> String {
    let max = width.saturating_sub(pad) as usize;
    match max {
        0 => String::new(), // nothing budgeted, nothing rendered — as `truncate_tail`
        _ if s.chars().count() > max => s.chars().take(max - 1).chain(std::iter::once('…')).collect(),
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
        // Only an exact home or a `/`-bounded child abbreviates — a bare prefix
        // match would fold a sibling like `/home/devops` into `~ops`.
        Ok(home) if path == home || path.strip_prefix(&home).is_some_and(|r| r.starts_with('/')) => {
            path.replacen(&home, "~", 1)
        }
        _ => path.to_string(),
    }
}

/// Session file under XDG state home: one shell per line as
/// `[>][!][@][+][*]folder\tcommand`. A line without `+` starts a new tab (its
/// parent); `+` lines are that tab's subshells in order. `>` marks the active
/// tab, `!` a favorite tab, `@` a tab whose auto feature is on (both parent
/// line only — absent means off, the default), `*` the active shell within its
/// tab, and the command is omitted when only the shell itself ran. Paths are
/// absolute, so the markers are unambiguous. A leading `-` is the pre-v0.4
/// spelling of "auto off", still read so older files restore their tabs.
/// A line starting with `#` is an app-wide setting rather than a shell:
/// `#copy off` records copy mode switched off (on is the default and is not
/// written).
const COPY_OFF_LINE: &str = "#copy off";

fn session_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("ricon").join("session"))
}

/// The session as last saved: its tabs in order, each with its shells (none
/// if nothing was saved), and the copy-mode setting.
fn load_session() -> Session {
    let text = session_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let mut tabs: Vec<TabState> = Vec::new();
    let copy_mode = !text.lines().any(|l| l == COPY_OFF_LINE);
    for line in text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
        // Strip the leading marker chars (order-independent); the path that
        // follows is absolute, so it never begins with one of them.
        let (mut active_tab, mut is_sub, mut active_shell, mut favorite) = (false, false, false, false);
        let mut auto = false;
        let mut rest = line;
        loop {
            match rest.chars().next() {
                Some('>') => active_tab = true,
                Some('+') => is_sub = true,
                Some('*') => active_shell = true,
                Some('!') => favorite = true,
                Some('@') => auto = true,
                // Pre-v0.4 "auto off"; off is the default now, so it only has
                // to be swallowed for the path behind it to parse.
                Some('-') => {}
                _ => break,
            }
            rest = &rest[1..];
        }
        let (cwd, cmd) = match rest.split_once('\t') {
            Some((cwd, cmd)) => (cwd, Some(cmd.to_string())),
            None => (rest, None),
        };
        let shell = ShellState { cwd: cwd.into(), cmd };
        match tabs.last_mut() {
            // A `+` line extends the current tab; anything else starts a new one.
            Some(tab) if is_sub => {
                if active_shell {
                    tab.active_shell = tab.shells.len();
                }
                tab.shells.push(shell);
            }
            _ => tabs.push(TabState {
                shells: vec![shell],
                active_shell: 0,
                active: active_tab,
                favorite,
                auto,
            }),
        }
    }
    Session { tabs, copy_mode }
}

/// Flatten `s` to a single session-file field: the format is one line per
/// shell, split once on a tab, so a newline or tab inside a path or a command
/// would be read back as a field boundary. A pasted multi-line command then
/// reopened as extra tabs rooted at whatever its second line happened to say.
/// Both characters become a space — the value is a label to restore, and one
/// that reads correctly beats one that parses back into a different session.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r', '\t'], " ")
}

fn save_session(session: &Session) {
    let Some(path) = session_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut body: Vec<String> = Vec::new();
    if !session.copy_mode {
        body.push(COPY_OFF_LINE.to_string());
    }
    for tab in &session.tabs {
        for (i, shell) in tab.shells.iter().enumerate() {
            // `>` active tab, `!` favorite, `@` auto on (parent line only),
            // `+` subshell, `*` active shell.
            let mut mark = String::new();
            if i == 0 && tab.active {
                mark.push('>');
            }
            if i == 0 && tab.favorite {
                mark.push('!');
            }
            if i == 0 && tab.auto {
                mark.push('@');
            }
            if i > 0 {
                mark.push('+');
            }
            if i == tab.active_shell {
                mark.push('*');
            }
            let cwd = one_line(&shell.cwd.display().to_string());
            body.push(match &shell.cmd {
                Some(cmd) => format!("{mark}{cwd}\t{}", one_line(cmd)),
                None => format!("{mark}{cwd}"),
            });
        }
    }
    // Staged, then renamed into place: a crash or a full disk mid-write must
    // leave the previous session, never a truncated one. (No fsync — this runs
    // on the UI thread, and a rename over the old file is flushed by the
    // filesystem's own replace-on-rename handling.)
    let staged = path.with_extension("new");
    if std::fs::write(&staged, body.join("\n")).is_ok() {
        let _ = std::fs::rename(&staged, &path);
    } else {
        let _ = std::fs::remove_file(&staged);
    }
}

/// A timestamp `ago` in the past, used to arm a throttled check so it fires on
/// the very next pass. `Instant` is monotonic from boot and subtracting past
/// that origin panics, so a machine up for less than `ago` gets `now` instead:
/// the check then simply fires one interval later rather than taking the app
/// down at startup.
fn stale(ago: Duration) -> Instant {
    let now = Instant::now();
    now.checked_sub(ago).unwrap_or(now)
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
}

/// Shells whose typed command lines `replay` captures (kata app.md §93: "bash
/// or any other shell").
const SHELLS: &[&str] =
    &["sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "ash", "nu", "elvish", "xonsh"];

/// Is `comm` (a /proc process name, possibly a login shell's leading `-`) a
/// shell whose prompt input `replay` should track?
fn is_shell(comm: &str) -> bool {
    let name = comm.rsplit('/').next().unwrap_or(comm);
    SHELLS.contains(&name.strip_prefix('-').unwrap_or(name))
}

/// What the auto feature types after a compaction (kata ai.md): the text of
/// `.ai/auto.md` in the project `dir`, else of the user-wide
/// `$XDG_CONFIG_HOME/ricon/auto.md` (`~/.config/ricon/auto.md`), whichever
/// exists and says anything first; else the plain word `continue`.
/// Surrounding blank lines are dropped; the rest is typed as it is, line
/// breaks included (a bracketed paste carries them into the client's composer
/// as text, not as Enters).
fn continue_text(dir: Option<&Path>) -> String {
    let project = dir.map(|d| d.join(AUTO_FILE));
    [project, global_auto_file()]
        .into_iter()
        .flatten()
        .find_map(|file| {
            std::fs::read_to_string(file).ok().map(|text| text.trim().to_string()).filter(|t| !t.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_CONTINUE.to_string())
}

/// The user-wide continue text: `$XDG_CONFIG_HOME/ricon/auto.md`, else
/// `~/.config/ricon/auto.md`.
fn global_auto_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("ricon").join("auto.md"))
}

/// The activity spinner's single color, per the kata: white.
const SPINNER_COLOR: Color = Color::Rgb(255, 255, 255);

/// The `replay` button's color, per the kata: red (bright enough to stay
/// legible on the dark tab backgrounds).
const REPLAY_COLOR: Color = Color::Rgb(255, 92, 92);

/// The `replay` button glyph, per the kata: an emoji representing replay.
const REPLAY_LABEL: &str = "🔁";
/// Terminal columns the replay emoji occupies — its clickable width (the emoji
/// renders two cells wide, so a one-char span would miss its right half).
const REPLAY_COLS: usize = 2;
/// Columns the replay row spends before the command text: the gutter, a
/// four-space indent, the two-cell icon, and a trailing space.
const REPLAY_PAD: u16 = 8;
/// First column of the replay icon — `{bar}    🔁` puts it one space past the
/// process row's `└` (kept in sync with `replay_row`).
const REPLAY_ICON_COL: usize = 5;

/// Pasted text as a terminal types it: line breaks become the CR an Enter
/// sends, and the paste markers are removed — a `ESC[201~` inside the text
/// would end the bracketed paste early and run the rest as keystrokes. Host
/// terminals filter this themselves; a middle-click paste is ricon's own.
fn paste_text(text: &str) -> String {
    text.replace("\x1b[200~", "").replace("\x1b[201~", "").replace("\r\n", "\r").replace('\n', "\r")
}

/// The control byte xterm sends for Ctrl+`c`. Most keys are the letter's low
/// five bits, but the symbols between them are not: hosts send Ctrl+\\ as
/// 0x1c, which crossterm reports as Ctrl+4 (and Ctrl+Space as Ctrl+' ') — so
/// the digits map back to the bytes they stand for, or Ctrl+\\ (SIGQUIT) would
/// reach the shell as Ctrl+T.
fn ctrl_byte(c: char) -> u8 {
    match c {
        ' ' | '2' | '@' => 0x00,
        '3'..='7' => c as u8 - b'3' + 0x1b,
        '8' | '?' => 0x7f,
        '/' => 0x1f,
        _ => (c.to_ascii_uppercase() as u8) & 0x1f,
    }
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
        KeyCode::Char(c) if ctrl && c.is_ascii() => vec![ctrl_byte(c)],
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

/// Modifiers that hand the pane's left button to ricon over an app that
/// grabbed the mouse. Alt is the one that survives the host terminal; Shift is
/// honoured too, for the hosts that do pass it through.
const BYPASS: KeyModifiers = KeyModifiers::ALT.union(KeyModifiers::SHIFT);

/// Selection highlight: a solid block rather than a reversal, so it reads the
/// same over plain and inverse text alike.
const SELECT_BG: Color = Color::Rgb(38, 90, 150);
const SELECT_FG: Color = Color::Rgb(255, 255, 255);

/// Columns of the word around `col` — the run of word characters, taking the
/// punctuation that holds paths, URLs, flags and identifiers together as part
/// of the word (double-clicking a path should yield the path, not one segment).
/// A cell that is not part of a word selects just itself.
fn word_span(chars: &[char], col: u16) -> (u16, u16) {
    fn is_word(c: &char) -> bool {
        c.is_alphanumeric() || "_-./~:@+=#%&?,\\".contains(*c)
    }
    let at = col as usize;
    if !chars.get(at).is_some_and(is_word) {
        return (col, col);
    }
    let from = chars[..at].iter().rposition(|c| !is_word(c)).map_or(0, |i| i + 1);
    let to = chars[at..].iter().position(|c| !is_word(c)).map_or(chars.len(), |i| at + i) - 1;
    (from as u16, to as u16)
}

/// Order two (row, col) cells into reading order (top-to-bottom, left-to-right).
fn order(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Top-left and bottom-right corners of the rectangle `a` and `b` span —
/// each axis sorted on its own, so a block dragged toward the top-right or
/// bottom-left is the same rectangle as one dragged the other way.
fn corners(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    ((a.0.min(b.0), a.1.min(b.1)), (a.0.max(b.0), a.1.max(b.1)))
}

/// The (row, col) cells covered by a reading-order selection from `a` to `b`
/// (inclusive) on a grid `cols` wide — a partial first row, full middle rows,
/// and a partial last row, matching what `contents_between` returns as text.
/// Yielded lazily: a whole-screen selection is tens of thousands of cells, and
/// the render path walks them every frame.
fn selection_cells(a: (u16, u16), b: (u16, u16), cols: u16) -> impl Iterator<Item = (u16, u16)> {
    let single = (a.0 == b.0).then(|| (a.1..=b.1).map(move |c| (a.0, c))).into_iter().flatten();
    let head = (a.0 < b.0).then(|| (a.1..cols).map(move |c| (a.0, c))).into_iter().flatten();
    let middle = (a.0 + 1..b.0).flat_map(move |r| (0..cols).map(move |c| (r, c)));
    let tail = (a.0 < b.0).then(|| (0..=b.1).map(move |c| (b.0, c))).into_iter().flatten();
    single.chain(head).chain(middle).chain(tail)
}

/// The (row, col) cells of a block (rectangular) selection from `a` to `b`
/// (inclusive) — every cell in the rectangle, row by row. This is what a
/// Shift+Ctrl+drag highlights and copies.
fn block_cells(a: (u16, u16), b: (u16, u16)) -> impl Iterator<Item = (u16, u16)> {
    let (a, b) = corners(a, b);
    (a.0..=b.0).flat_map(move |r| (a.1..=b.1).map(move |c| (r, c)))
}

/// Text of a block selection: each row's columns from `a.1` to `b.1` (inclusive)
/// joined by a newline, so the clipboard holds exactly the rectangle that was
/// highlighted. Trailing blank space on each row is dropped, matching the
/// reading-order copy.
fn block_text(shell: &Shell, a: (u16, u16), b: (u16, u16), cols: u16) -> String {
    let (a, b) = corners(a, b);
    let mut out = String::new();
    for r in a.0..=b.0 {
        let row = shell.screen_between((r, a.1), (r, (b.1 + 1).min(cols)));
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(row.trim_end());
    }
    out
}

/// Put `text` on the clipboard by every route available, because no single one
/// covers every host: OSC 52 is the only path that works over SSH and is what
/// kitty/wezterm/foot/tmux honour, while VTE-based terminals (gnome-terminal,
/// Tilix, Terminator — VTE has never implemented OSC 52 writes) drop it on the
/// floor and are only reachable through the local X11/Wayland selection.
/// Both are best-effort: a failing route never masks the other, and a copy that
/// no route accepts is not an error worth tearing the app down for.
fn copy_clipboard(text: &str) {
    copy_osc52(text);
    desktop_clipboard_send(text);
}

/// The process-wide desktop clipboard, or `None` with no local display (plain
/// SSH — OSC 52 carries the copy there). X11 hands the selection to a *live*
/// owner, so the handle must outlive the copy: it is kept here for the life of
/// the process, and pasting keeps working as long as ricon runs.
///
/// The clipboard is owned by a single dedicated thread. arboard's own contract
/// is that a `Clipboard` is only ever operated on one thread at a time, and a
/// dead or slow X11/Wayland selection owner can make `set_text` block — so the
/// write must never run on the render thread, where it would freeze the whole
/// app mid-handle. The UI posts text over a channel; the worker thread does the
/// (possibly blocking) write, and the render path never waits on it.
fn desktop_clipboard_send(text: &str) {
    // Tests must not clobber the developer's real clipboard.
    if !cfg!(test) {
        clipboard_thread().send(Clip::Set(text.to_string()));
    }
}

/// Read the desktop's primary selection (what a middle click pastes on X11
/// and Wayland; the clipboard when there is none) and hand it to `reply`.
/// Off-thread like the write: a read waits on the selection's owner.
fn desktop_clipboard_get(reply: mpsc::Sender<String>) {
    if !cfg!(test) {
        clipboard_thread().send(Clip::Get(reply));
    }
}

/// One job for the clipboard thread.
enum Clip {
    Set(String),
    Get(mpsc::Sender<String>),
}

/// The channel to the process-wide clipboard thread, started on first use. A
/// job posted to it is dropped when there is no local display (plain SSH —
/// OSC 52 carries the copy there, and a middle click then pastes nothing).
fn clipboard_thread() -> &'static ClipboardPost {
    static CLIPBOARD: OnceLock<ClipboardPost> = OnceLock::new();
    CLIPBOARD.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Clip>();
        // The clipboard is created on the worker thread so it is *owned* there
        // and never touched anywhere else — the one-thread rule arboard needs.
        thread::spawn(move || {
            use arboard::{GetExtLinux, LinuxClipboardKind, SetExtLinux};
            let Ok(mut clipboard) = arboard::Clipboard::new() else { return };
            while let Ok(job) = rx.recv() {
                match job {
                    // Both selections, as a terminal does: the clipboard for
                    // Ctrl+V, the primary for a middle click elsewhere.
                    Clip::Set(text) => {
                        let _ = clipboard.set().clipboard(LinuxClipboardKind::Primary).text(&text);
                        let _ = clipboard.set_text(&text);
                    }
                    Clip::Get(reply) => {
                        let text = clipboard
                            .get()
                            .clipboard(LinuxClipboardKind::Primary)
                            .text()
                            .or_else(|_| clipboard.get_text());
                        if let Ok(text) = text
                            && !text.is_empty()
                        {
                            let _ = reply.send(text);
                        }
                    }
                }
            }
        });
        ClipboardPost(tx)
    })
}

/// Sender to the clipboard thread; a send that fails (the thread quit for
/// want of a display) is simply a copy or paste that went nowhere.
struct ClipboardPost(mpsc::Sender<Clip>);

impl ClipboardPost {
    fn send(&self, job: Clip) {
        let _ = self.0.send(job);
    }
}

/// The desktop ping for an idle agent (kata ai.md): a bell, then the
/// notification sequences the host terminals speak — OSC 9 (iTerm2, WezTerm,
/// ConEmu), OSC 777 (urxvt, foot, VTE builds that carry the patch) and OSC 99
/// (kitty). A terminal ignores the ones it does not know, so all go out.
fn idle_notice(agent: &str, folder: &str) -> String {
    let body = format!("{agent} in {folder} is waiting for you");
    format!(
        "\x07\x1b]9;ricon: {body}\x07\x1b]777;notify;ricon;{body}\x07\x1b]99;i=1:p=title;ricon\x1b\\\x1b]99;i=1:p=body;{body}\x1b\\"
    )
}

/// Write a control sequence straight to the host terminal, between frames
/// (cursor-neutral, like the OSC 52 copy). Silent under `cargo test`.
fn notify_host(seq: &str) {
    if cfg!(test) {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(seq.as_bytes()).and_then(|()| out.flush());
}

/// Push `text` to the host terminal's clipboard with an OSC 52 sequence,
/// written straight to stdout (inline, cursor-neutral — safe between frames).
/// Silent under `cargo test`, where stdout is the test report.
fn copy_osc52(text: &str) {
    if cfg!(test) {
        return;
    }
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(seq.as_bytes()).and_then(|()| out.flush());
}

/// Standard (RFC 4648) base64 with padding — no dependency, OSC 52 wants it.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    out
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
            // Coordinate byte is 32 + 1-based position, capped at 255 — the
            // largest position legacy encoding can express is 223.
            let clamp = |v: u16| 32 + (v + 1).min(223) as u8;
            vec![0x1b, b'[', b'M', 32 + cb, clamp(col), clamp(row)]
        }
    })
}

#[cfg(test)]
mod tests;
