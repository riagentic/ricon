//! Full behavioral test suite. Every user-visible rule from `.katana/` has a
//! test here: pure helpers, theming, key/mouse encoding, session persistence,
//! sidebar layout math, live-PTY end-to-end behavior, and UI rendering
//! (asserted on real `TestBackend` buffers, colors and modifiers included).

use super::*;
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Modifier};
use std::sync::Mutex;

// ── harness ──────────────────────────────────────────────────────────────────

/// Serializes every test that touches process-global env (`XDG_STATE_HOME` and
/// `HOME` are the only vars mutated): writers hold it while the var is swapped,
/// readers hold it so they never observe another test's temporary value.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn with_env_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    f()
}

fn with_state_home<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let saved = std::env::var_os("XDG_STATE_HOME");
    unsafe { std::env::set_var("XDG_STATE_HOME", dir) };
    let out = f();
    match saved {
        Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
    }
    out
}

/// Same guard, for the user-wide `auto.md` under `$XDG_CONFIG_HOME`.
fn with_config_home<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let saved = std::env::var_os("XDG_CONFIG_HOME");
    unsafe { std::env::set_var("XDG_CONFIG_HOME", dir) };
    let out = f();
    match saved {
        Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
    }
    out
}

/// Same guard, for the `$HOME`-relative model sources.
fn with_home<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let saved = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", dir) };
    let out = f();
    match saved {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    out
}

fn wait_for(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn test_shell(cwd: &Path) -> Shell {
    Shell::spawn(24, 80, cwd, None).expect("spawn test shell")
}

/// Type `cmd` at a live shell's prompt exactly as a user would — every byte
/// routed through `note_input` (ricon's capture path) then the PTY — and
/// confirm with Enter, waiting for the echo so the anchor and commit reads see
/// a settled screen. Mirrors what `App::write_active` does per keystroke.
fn type_and_enter(shell: &mut Shell, cmd: &str) {
    for b in cmd.bytes() {
        shell.note_input(&[b]);
        shell.send(&[b]);
    }
    assert!(
        wait_for(|| screen_contents(shell).contains(cmd), Duration::from_secs(10)),
        "typed line echoed, got: {:?}",
        screen_contents(shell)
    );
    shell.note_input(b"\r");
    shell.send(b"\r");
}

/// Same guard, for the host-terminal vars a spawned shell must not inherit.
fn with_host_terminal_vars<T>(f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let saved: Vec<_> = HOST_TERMINAL_VARS.iter().map(|v| (*v, std::env::var_os(v))).collect();
    for var in HOST_TERMINAL_VARS {
        unsafe { std::env::set_var(var, "1234") };
    }
    let out = f();
    for (var, value) in saved {
        match value {
            Some(v) => unsafe { std::env::set_var(var, v) },
            None => unsafe { std::env::remove_var(var) },
        }
    }
    out
}

fn screen_contents(shell: &Shell) -> String {
    shell.parser.lock().unwrap_or_else(PoisonError::into_inner).screen().contents()
}

/// An `App` with one tab per entry, each holding that many shells, all in the
/// current dir. Like `App::new`, the search row starts focused, copy mode is
/// on and `shown` is pre-filtered.
fn test_app(shell_counts: &[usize]) -> App {
    let base = std::env::current_dir().expect("cwd");
    let mut tabs = Vec::new();
    for &n in shell_counts {
        let mut tab = Tab::spawn(24, 80, &base, None).expect("spawn tab");
        for _ in 1..n {
            tab.shells.push(test_shell(&base));
        }
        tabs.push(tab);
    }
    let mut app = App {
        tabs,
        active: 0,
        swallow_release: false,
        pty_rows: 24,
        pty_cols: 80,
        term_width: 80,
        sidebar_width: SIDEBAR_WIDTH,
        dragging_sidebar: false,
        dragging_tab: None,
        list_offset: 0,
        sidebar_rows: 1000,
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
        copied_at: None,
        agent_probe: AgentProbe::spawn(),
        agent_cursor: 0,
        drawn: Instant::now() - POLL_INTERVAL,
        quit: false,
        confirm_quit: None,
        help: false,
    };
    app.refresh_shown();
    app
}

fn render(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    term.draw(|frame| draw(frame, app)).expect("draw");
    term.backend().buffer().clone()
}

fn row_text(buf: &Buffer, y: u16, width: u16) -> String {
    (0..width).map(|x| cell(buf, x, y).symbol()).collect()
}

fn cell(buf: &Buffer, x: u16, y: u16) -> &ratatui::buffer::Cell {
    buf.cell((x, y)).expect("cell in bounds")
}

fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

// ── pure helpers ─────────────────────────────────────────────────────────────

#[test]
fn truncate_tail_keeps_short_strings() {
    assert_eq!(truncate_tail("abc", 26, 5), "abc");
    assert_eq!(truncate_tail("", 26, 5), "");
}

#[test]
fn base64_matches_rfc4648_vectors() {
    // Padding at every residue, plus the two clipboard-relevant bytes 62/63.
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64(&[0xff, 0xef]), "/+8=");
}

#[test]
fn selection_cells_are_reading_order() {
    let cells = |a, b, cols| selection_cells(a, b, cols).collect::<Vec<_>>();
    // Single row: an inclusive column span.
    assert_eq!(cells((2, 3), (2, 5), 10), [(2, 3), (2, 4), (2, 5)]);
    // Multi row: tail of the first row, full middle rows, head of the last.
    assert_eq!(cells((0, 2), (2, 1), 3), [(0, 2), (1, 0), (1, 1), (1, 2), (2, 0), (2, 1)]);
    // Two adjacent rows: no middle rows to walk.
    assert_eq!(cells((0, 1), (1, 1), 3), [(0, 1), (0, 2), (1, 0), (1, 1)]);
}

#[test]
fn block_cells_cover_the_rectangle_row_by_row() {
    // A block selection is every cell in the rectangle between the two corners,
    // in reading order — the column of a table or a log, not whole rows.
    let cells = |a, b| block_cells(a, b).collect::<Vec<_>>();
    assert_eq!(
        cells((1, 2), (3, 4)),
        [(1, 2), (1, 3), (1, 4), (2, 2), (2, 3), (2, 4), (3, 2), (3, 3), (3, 4)]
    );
    // Corners in either order normalise to the same rectangle.
    assert_eq!(cells((3, 4), (1, 2)), cells((1, 2), (3, 4)));
    // A single row is just a column span; a single cell is itself.
    assert_eq!(cells((0, 0), (0, 2)), [(0, 0), (0, 1), (0, 2)]);
    assert_eq!(cells((2, 2), (2, 2)), [(2, 2)]);
}

#[test]
fn word_span_takes_whole_paths_urls_and_flags() {
    let chars: Vec<char> = "cd ~/code/gen/ricon && git switch -b feat/x".chars().collect();
    let span = |col: u16| {
        let (a, b) = word_span(&chars, col);
        chars[a as usize..=b as usize].iter().collect::<String>()
    };
    assert_eq!(span(0), "cd", "a bare word");
    assert_eq!(span(8), "~/code/gen/ricon", "a whole path, not one segment");
    assert_eq!(span(41), "feat/x", "a branch name");
    assert_eq!(span(34), "-b", "a flag keeps its dash");
    assert_eq!(span(2), " ", "a blank cell selects only itself");
    // Edges and out-of-range columns are safe.
    assert_eq!(word_span(&chars, 0).0, 0);
    assert_eq!(word_span(&chars, chars.len() as u16), (chars.len() as u16, chars.len() as u16));
    assert_eq!(word_span(&[], 0), (0, 0));
    let url: Vec<char> = "see https://example.com/a?b=1#c.".chars().collect();
    let (a, b) = word_span(&url, 10);
    assert_eq!(url[a as usize..=b as usize].iter().collect::<String>(), "https://example.com/a?b=1#c.");
}

#[test]
fn buttons_share_one_hit_test_width() {
    // Render and hit-test agree only while both labels are exactly as wide as
    // the columns `copy_button_x` reserves.
    assert_eq!(COPY_ON_BUTTON.chars().count() as u16, BUTTON_COLS);
    assert_eq!(COPY_OFF_BUTTON.chars().count() as u16, BUTTON_COLS);
}

#[test]
fn copy_button_x_is_flush_left_of_the_version() {
    let version = concat!("v", env!("CARGO_PKG_VERSION"), " ").chars().count() as u16;
    let btn = BUTTON_COLS;
    assert_eq!(copy_button_x(80), Some((80 - version - btn, 80 - version)));
    // Too narrow to leave any left segment: dropped rather than squeezed.
    assert_eq!(copy_button_x(version + btn), None);
    assert_eq!(copy_button_x(version), None);
    assert_eq!(copy_button_x(0), None);
    assert_eq!(copy_button_x(version + btn + 1), Some((1, btn + 1)));
}

#[test]
fn order_normalizes_into_reading_order() {
    assert_eq!(order((5, 0), (1, 9)), ((1, 9), (5, 0)));
    assert_eq!(order((2, 7), (2, 3)), ((2, 3), (2, 7)));
}

#[test]
fn truncate_tail_elides_head_with_ellipsis() {
    // width 8, pad 5 → 3 columns → "…" plus the last two chars. The budget is
    // a ceiling: what follows on the row (the auto button, the panel border)
    // sits at a fixed column and must never be pushed out of place.
    assert_eq!(truncate_tail("abcdef", 8, 5), "…ef");
    assert_eq!(truncate_tail("abc", 8, 5), "abc", "a name that fits is untouched");
    // One column is exactly the ellipsis and nothing else.
    assert_eq!(truncate_tail("abcdef", 6, 5), "…");
    // width <= pad budgets nothing, so nothing is rendered: an `…` here would
    // be the very overflow the ceiling exists to prevent, pushing the auto
    // button off the column its hit-test reads.
    assert_eq!(truncate_tail("abcdef", 5, 5), "");
    assert_eq!(truncate_tail("abcdef", 3, 5), "", "an underflowing width budgets nothing too");
}

#[test]
fn truncate_tail_is_char_safe_on_multibyte() {
    assert_eq!(truncate_tail("ééééééé", 8, 5), "…éé");
}

#[test]
fn truncate_head_keeps_command_start_and_is_char_safe() {
    // Fits within width - pad: unchanged (a command reads from its front).
    assert_eq!(truncate_head("cargo test", 20, 7), "cargo test");
    // Overflows: keep the head, mark the elided tail with a single `…`.
    assert_eq!(truncate_head("cargo test --all-features", 15, 7), "cargo t…");
    assert!(truncate_head("cargo test --all-features", 15, 7).chars().count() <= 8);
    // One column is exactly the ellipsis and nothing else.
    assert_eq!(truncate_head("anything", 8, 7), "…");
    // width <= pad budgets nothing, so nothing is rendered (as `truncate_tail`).
    assert_eq!(truncate_head("anything", 7, 7), "");
    // Multibyte: truncates on char boundaries, never mid-byte.
    assert_eq!(truncate_head("caféééééé", 12, 7), "café…");
}

#[test]
fn expand_home_handles_tilde_forms() {
    with_env_lock(|| {
        let home = std::env::var("HOME").expect("HOME set");
        assert_eq!(expand_home("~"), PathBuf::from(&home));
        assert_eq!(expand_home("~/x"), PathBuf::from(format!("{home}/x")));
        assert_eq!(expand_home("~user/x"), PathBuf::from("~user/x"));
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    });
}

#[test]
fn abbreviate_home_shortens_only_home_prefix() {
    with_env_lock(|| {
        let home = std::env::var("HOME").expect("HOME set");
        assert_eq!(abbreviate_home(&format!("{home}/code")), "~/code");
        assert_eq!(abbreviate_home(&home), "~");
        assert_eq!(abbreviate_home("/usr/lib"), "/usr/lib");
        // A sibling sharing the home prefix but not `/`-bounded is left intact.
        assert_eq!(abbreviate_home(&format!("{home}ext")), format!("{home}ext"));
    });
}

#[test]
fn folder_name_is_last_component() {
    assert_eq!(folder_name(Path::new("/a/b/c")), "c");
    assert_eq!(folder_name(Path::new("/")), "/");
}

#[test]
fn base_path_defaults_to_current_dir() {
    // Kata app.md: base path is the working directory the app started from.
    assert_eq!(base_path(None).expect("cwd"), std::env::current_dir().expect("cwd"));
}

#[test]
fn base_path_canonicalizes_first_parameter() {
    // Kata app.md: base path derives from the first parameter when given.
    let home = std::env::var("HOME").expect("HOME set");
    assert_eq!(base_path(Some("~".into())).expect("home"), std::fs::canonicalize(home).expect("home"));
    assert!(base_path(Some("/definitely/not/a/dir".into())).is_err());
}

#[test]
fn json_string_extracts_exact_keys_only() {
    assert_eq!(json_string(r#"{"model":"opus-4"}"#, "model"), Some("opus-4".into()));
    assert_eq!(json_string(r#"{"models":"x"}"#, "model"), None);
    assert_eq!(json_string(r#"{"model":""}"#, "model"), None);
    assert_eq!(json_string("not json", "model"), None);
}

#[test]
fn read_tail_returns_last_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("log");
    std::fs::write(&path, b"0123456789").expect("write");
    assert_eq!(read_tail(&path, 4).expect("tail"), b"6789");
    assert_eq!(read_tail(&path, 100).expect("tail"), b"0123456789");
}

#[test]
fn pty_size_maps_rows_cols() {
    let size = pty_size(24, 80);
    assert_eq!((size.rows, size.cols, size.pixel_width, size.pixel_height), (24, 80, 0, 0));
}

#[test]
fn default_shell_is_never_empty() {
    assert!(!default_shell().is_empty());
}

#[test]
fn is_shell_matches_shells_including_login_and_paths() {
    assert!(is_shell("bash"));
    assert!(is_shell("-zsh")); // login shell's leading dash
    assert!(is_shell("/usr/bin/fish")); // full path
    assert!(!is_shell("vim"));
    assert!(!is_shell("python3"));
}

#[test]
fn version_is_0_4_0() {
    // Kata meta.md: ricon app version is 0.4.0.
    assert_eq!(env!("CARGO_PKG_VERSION"), "0.4.0");
}

// ── theming ──────────────────────────────────────────────────────────────────

#[test]
fn active_tab_pastel_is_light_and_soft_under_dark_text() {
    // Kata app.md: one pastel for the active tab — light and low in
    // saturation — and text dark enough to read on it.
    let Color::Rgb(r, g, b) = ACTIVE_BG else { panic!("the pastel is an RGB color") };
    let (r, g, b) = (u16::from(r), u16::from(g), u16::from(b));
    assert!(r + g + b > 3 * 170, "light: {ACTIVE_BG:?}");
    assert!(r.max(g).max(b) - r.min(g).min(b) < 80, "soft, not saturated: {ACTIVE_BG:?}");
    let Color::Rgb(r, g, b) = ACTIVE_FG else { panic!("the text color is an RGB color") };
    assert!(u16::from(r) + u16::from(g) + u16::from(b) < 3 * 70, "dark text: {ACTIVE_FG:?}");
}

#[test]
fn sweep_always_lights_something_and_walks_left_to_right() {
    // The lit segment enters from the left, leaves to the right, and at no
    // phase is the whole bar dark — a busy tab must always show it moving.
    let cells = 24;
    let period = cells + BAR_LEN - 1;
    let mut last_from = 0;
    for step in 0..period * 2 {
        let (from, to) = sweep(BAR_STEP * step as u32, cells);
        assert!(from < to && to <= cells, "step {step}: [{from}, {to})");
        assert!(to - from <= BAR_LEN, "step {step}: no longer than the segment");
        assert!(from >= last_from || from == 0, "step {step}: moves right, then wraps");
        last_from = from;
    }
    assert_eq!(sweep(Duration::ZERO, cells).1, 1, "starts with one lit cell at the left edge");
    assert_eq!(sweep(BAR_STEP * (period - 1) as u32, cells), (cells - 1, cells), "ends at the right edge");
}

#[test]
fn spinner_color_is_white() {
    // Kata app.md: the activity spinner is white.
    assert_eq!(SPINNER_COLOR, Color::Rgb(255, 255, 255));
}

// ── key encoding ─────────────────────────────────────────────────────────────

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

#[test]
fn encode_key_plain_and_ctrl_chars() {
    assert_eq!(encode_key(&key(KeyCode::Char('a'), KeyModifiers::NONE), false), Some(b"a".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), false), Some(vec![0x03]));
    assert_eq!(encode_key(&key(KeyCode::Char('['), KeyModifiers::CONTROL), false), Some(vec![0x1b]));
}

#[test]
fn encode_key_alt_prefixes_escape_on_text_keys() {
    assert_eq!(encode_key(&key(KeyCode::Char('a'), KeyModifiers::ALT), false), Some(b"\x1ba".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Enter, KeyModifiers::ALT), false), Some(b"\x1b\r".to_vec()));
}

#[test]
fn encode_key_basic_controls() {
    assert_eq!(encode_key(&key(KeyCode::Enter, KeyModifiers::NONE), false), Some(b"\r".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Backspace, KeyModifiers::NONE), false), Some(vec![0x7f]));
    assert_eq!(encode_key(&key(KeyCode::Tab, KeyModifiers::NONE), false), Some(b"\t".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Tab, KeyModifiers::SHIFT), false), Some(b"\x1b[Z".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::BackTab, KeyModifiers::NONE), false), Some(b"\x1b[Z".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Esc, KeyModifiers::NONE), false), Some(vec![0x1b]));
}

#[test]
fn encode_key_cursor_keys_honor_decckm() {
    // Kata transparency: DECCKM (application cursor keys) must be respected.
    assert_eq!(encode_key(&key(KeyCode::Up, KeyModifiers::NONE), false), Some(b"\x1b[A".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Up, KeyModifiers::NONE), true), Some(b"\x1bOA".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Left, KeyModifiers::NONE), true), Some(b"\x1bOD".to_vec()));
}

#[test]
fn encode_key_cursor_keys_carry_xterm_modifiers() {
    // Modified cursor keys use CSI 1;m even in application mode.
    assert_eq!(encode_key(&key(KeyCode::Up, KeyModifiers::CONTROL), true), Some(b"\x1b[1;5A".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Right, KeyModifiers::SHIFT), false), Some(b"\x1b[1;2C".to_vec()));
    assert_eq!(
        encode_key(&key(KeyCode::Down, KeyModifiers::ALT | KeyModifiers::CONTROL), false),
        Some(b"\x1b[1;7B".to_vec())
    );
}

#[test]
fn encode_key_tilde_keys() {
    assert_eq!(encode_key(&key(KeyCode::PageUp, KeyModifiers::NONE), false), Some(b"\x1b[5~".to_vec()));
    assert_eq!(
        encode_key(&key(KeyCode::PageDown, KeyModifiers::CONTROL), false),
        Some(b"\x1b[6;5~".to_vec())
    );
    assert_eq!(encode_key(&key(KeyCode::Insert, KeyModifiers::NONE), false), Some(b"\x1b[2~".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Delete, KeyModifiers::NONE), false), Some(b"\x1b[3~".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::Home, KeyModifiers::NONE), false), Some(b"\x1b[H".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::End, KeyModifiers::NONE), false), Some(b"\x1b[F".to_vec()));
}

#[test]
fn encode_key_function_keys() {
    assert_eq!(encode_key(&key(KeyCode::F(1), KeyModifiers::NONE), false), Some(b"\x1bOP".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::F(4), KeyModifiers::NONE), false), Some(b"\x1bOS".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::F(1), KeyModifiers::SHIFT), false), Some(b"\x1b[1;2P".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::F(5), KeyModifiers::NONE), false), Some(b"\x1b[15~".to_vec()));
    assert_eq!(encode_key(&key(KeyCode::F(12), KeyModifiers::NONE), false), Some(b"\x1b[24~".to_vec()));
}

#[test]
fn encode_key_ignores_unmapped_keys() {
    assert_eq!(encode_key(&key(KeyCode::CapsLock, KeyModifiers::NONE), false), None);
}

// ── mouse encoding ───────────────────────────────────────────────────────────

fn modes(mode: MouseProtocolMode, encoding: MouseProtocolEncoding) -> TermModes {
    TermModes { app_cursor: false, bracketed_paste: false, mouse_mode: mode, mouse_encoding: encoding }
}

fn mouse(kind: MouseEventKind, mods: KeyModifiers) -> MouseEvent {
    MouseEvent { kind, column: 0, row: 0, modifiers: mods }
}

#[test]
fn encode_mouse_disabled_mode_swallows_everything() {
    // Kata transparency: never send mouse bytes an app didn't subscribe to.
    let m = modes(MouseProtocolMode::None, MouseProtocolEncoding::Sgr);
    let down = mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE);
    assert_eq!(encode_mouse(&down, 0, 0, &m), None);
}

#[test]
fn encode_mouse_sgr_press_and_release() {
    let m = modes(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Sgr);
    let down = mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), KeyModifiers::NONE);
    assert_eq!(encode_mouse(&down, 0, 0, &m), Some(b"\x1b[<0;1;1M".to_vec()));
    assert_eq!(encode_mouse(&up, 4, 9, &m), Some(b"\x1b[<0;5;10m".to_vec()));
}

#[test]
fn encode_mouse_motion_requires_motion_modes() {
    let drag = mouse(MouseEventKind::Drag(MouseButton::Left), KeyModifiers::NONE);
    let moved = mouse(MouseEventKind::Moved, KeyModifiers::NONE);
    let press_only = modes(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Sgr);
    let button = modes(MouseProtocolMode::ButtonMotion, MouseProtocolEncoding::Sgr);
    let any = modes(MouseProtocolMode::AnyMotion, MouseProtocolEncoding::Sgr);
    assert_eq!(encode_mouse(&drag, 0, 0, &press_only), None);
    assert_eq!(encode_mouse(&drag, 0, 0, &button), Some(b"\x1b[<32;1;1M".to_vec()));
    assert_eq!(encode_mouse(&moved, 0, 0, &button), None);
    assert_eq!(encode_mouse(&moved, 0, 0, &any), Some(b"\x1b[<35;1;1M".to_vec()));
}

#[test]
fn encode_mouse_wheel_and_modifiers() {
    let m = modes(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Sgr);
    let up = mouse(MouseEventKind::ScrollUp, KeyModifiers::NONE);
    let down = mouse(MouseEventKind::ScrollDown, KeyModifiers::NONE);
    let ctrl_click = mouse(MouseEventKind::Down(MouseButton::Right), KeyModifiers::CONTROL);
    assert_eq!(encode_mouse(&up, 0, 0, &m), Some(b"\x1b[<64;1;1M".to_vec()));
    assert_eq!(encode_mouse(&down, 0, 0, &m), Some(b"\x1b[<65;1;1M".to_vec()));
    assert_eq!(encode_mouse(&ctrl_click, 0, 0, &m), Some(b"\x1b[<18;1;1M".to_vec()));
}

#[test]
fn encode_mouse_legacy_encoding_and_clamp() {
    let m = modes(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Default);
    let down = mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), KeyModifiers::NONE);
    // press: cb 0 → 32; col/row 0 → 33.
    assert_eq!(encode_mouse(&down, 0, 0, &m), Some(vec![0x1b, b'[', b'M', 32, 33, 33]));
    // release loses button identity (cb 3 → 35).
    assert_eq!(encode_mouse(&up, 0, 0, &m), Some(vec![0x1b, b'[', b'M', 35, 33, 33]));
    // legacy coordinates saturate at byte 255 (position 223).
    assert_eq!(encode_mouse(&down, 500, 500, &m), Some(vec![0x1b, b'[', b'M', 32, 255, 255]));
}

// ── session persistence ──────────────────────────────────────────────────────

fn sample_states() -> Vec<TabState> {
    vec![
        TabState {
            shells: vec![
                ShellState { cwd: "/tmp/a".into(), cmd: Some("vim notes.txt".into()) },
                ShellState { cwd: "/tmp/b".into(), cmd: None },
            ],
            active_shell: 1,
            active: false,
            favorite: true,
            auto: false,
        },
        TabState {
            shells: vec![ShellState { cwd: "/tmp/c".into(), cmd: None }],
            active_shell: 0,
            active: true,
            favorite: false,
            auto: true,
        },
    ]
}

#[test]
fn session_roundtrip_preserves_everything() {
    // Kata app.md: tabs, folders, commands, favorites, active tab and active
    // shell are all persisted and restored.
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let session = Session { tabs: sample_states(), copy_mode: true };
        save_session(&session);
        assert_eq!(load_session(), session);
        // Copy mode off is the one app-wide setting the file carries (kata
        // ui.md); on is the default and is not written.
        let off = Session { copy_mode: false, ..session };
        save_session(&off);
        assert_eq!(load_session(), off);
        let text = std::fs::read_to_string(session_path().expect("path")).expect("file");
        assert!(text.starts_with(COPY_OFF_LINE), "the setting line leads the file: {text:?}");
    });
}

#[test]
fn session_empty_and_missing_files_load_as_no_tabs() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        assert_eq!(load_session(), Session::default());
        assert!(load_session().copy_mode, "copy mode is on for a fresh install");
        save_session(&Session::default());
        assert_eq!(load_session(), Session::default());
    });
}

#[test]
fn session_markers_parse_in_any_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let path = session_path().expect("path");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, ">!*@/tmp/x\tmake -j\n+/tmp/y\n").expect("write");
        let tabs = load_session().tabs;
        assert_eq!(tabs.len(), 1);
        assert!(tabs[0].active && tabs[0].favorite);
        assert!(tabs[0].auto, "the `@` marker turns the auto feature on");
        assert_eq!(tabs[0].active_shell, 0);
        assert_eq!(tabs[0].shells[0].cmd.as_deref(), Some("make -j"));
        assert_eq!(tabs[0].shells[1].cwd, PathBuf::from("/tmp/y"));
    });
}

#[test]
fn a_pre_v0_4_session_file_still_restores_its_tabs() {
    // `-` used to spell "auto off" back when auto was on by default. Off is
    // the default now, so the marker carries nothing — but it must still be
    // swallowed, or the path behind it parses as `-/tmp/x` and the tab is
    // dropped as a folder that no longer exists.
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let path = session_path().expect("path");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, ">!-*/tmp/x\n").expect("write");
        let tabs = load_session().tabs;
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].shells[0].cwd, PathBuf::from("/tmp/x"), "the path survives the marker");
        assert!(!tabs[0].auto, "and the tab keeps the auto feature off");
    });
}

#[test]
fn spawned_shells_do_not_inherit_the_host_terminal_identity() {
    // ricon is not the terminal that started it. Letting the host's identity
    // through is what made bash render wrong in here and `sh` look fine:
    // `VTE_VERSION` turns on GNOME's shell integration in /etc/profile.d, after
    // which bash writes OSC 7 and OSC 133 sequences this emulator implements
    // neither of — and dash sources none of it. `TERM` is the one terminal
    // identity ricon does vouch for, so it must survive.
    let dir = tempfile::tempdir().expect("tempdir");
    with_host_terminal_vars(|| {
        let shell = test_shell(dir.path());
        let pid = shell.pid.expect("shell pid");
        assert!(
            wait_for(|| env_var(pid, "TERM").is_some(), Duration::from_secs(10)),
            "the shell's environ becomes readable"
        );
        assert_eq!(env_var(pid, "TERM").as_deref(), Some("xterm-256color"));
        for var in HOST_TERMINAL_VARS {
            assert_eq!(env_var(pid, var), None, "{var} must not follow the shell in");
        }
    });
}

#[test]
fn resize_keeps_the_pty_and_the_screen_on_one_size() {
    // The child draws for whatever size the PTY reports, and the reader thread
    // lays those bytes into the screen the moment they land. The two must never
    // disagree — output meant for one width parsed into a grid on another wraps
    // into garbage that no later frame repairs, because the damage is in the
    // grid rather than in the paint.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut shell = test_shell(dir.path());
    for (rows, cols) in [(30u16, 120u16), (24, 60), (40, 200), (24, 80)] {
        shell.resize(rows, cols);
        assert_eq!(
            shell.parser.lock().unwrap_or_else(PoisonError::into_inner).screen().size(),
            (rows, cols),
            "the screen takes the new size"
        );
        let pty = shell.master.get_size().expect("pty size");
        assert_eq!((pty.rows, pty.cols), (rows, cols), "and the PTY reports the very same one");
    }
}

#[test]
fn session_survives_a_command_carrying_newlines_and_tabs() {
    // The file is one line per shell, split once on a tab. A pasted multi-line
    // command used to be written raw, so reloading read its continuation lines
    // as further tabs rooted at whatever they happened to say — the session
    // came back with tabs the user never opened.
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let states = vec![TabState {
            shells: vec![ShellState {
                cwd: "/tmp/a".into(),
                cmd: Some("for f in *; do\n\techo \"$f\"\ndone".into()),
            }],
            active_shell: 0,
            active: true,
            favorite: false,
            auto: true,
        }];
        save_session(&Session { tabs: states, copy_mode: true });
        let back = load_session().tabs;
        assert_eq!(back.len(), 1, "one shell persisted stays one tab, got {back:#?}");
        assert_eq!(back[0].shells.len(), 1);
        assert_eq!(back[0].shells[0].cwd, PathBuf::from("/tmp/a"));
        assert_eq!(
            back[0].shells[0].cmd.as_deref(),
            Some("for f in *; do  echo \"$f\" done"),
            "the command is flattened to one line, not split into new tabs"
        );
    });
}

#[test]
fn persist_now_writes_once_per_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let mut app = test_app(&[1]);
        app.persist_now();
        let first = app.saved_session.clone();
        assert_eq!(first.tabs.len(), 1);
        assert!(first.tabs[0].active);
        let path = session_path().expect("path");
        let written = std::fs::metadata(&path).expect("session file").modified().expect("mtime");
        app.persist_now(); // unchanged session → no rewrite
        assert_eq!(std::fs::metadata(&path).expect("session file").modified().expect("mtime"), written);
        assert_eq!(app.saved_session, first);
    });
}

#[test]
fn restore_tab_rebuilds_shells_and_flags() {
    let base = std::env::current_dir().expect("cwd");
    let mut app = test_app(&[1]);
    let state = TabState {
        shells: vec![
            ShellState { cwd: base.clone(), cmd: None },
            ShellState { cwd: base.clone(), cmd: None },
        ],
        active_shell: 1,
        active: true,
        favorite: true,
        auto: false,
    };
    app.restore_tab(&state).expect("restore");
    let tab = app.tabs.last().expect("restored tab");
    assert_eq!(tab.shells.len(), 2);
    assert_eq!(tab.active, 1);
    assert!(tab.favorite);
    assert!(!tab.auto, "the persisted auto choice is restored");
    assert_eq!(app.active, app.tabs.len() - 1);
}

// ── git branch ───────────────────────────────────────────────────────────────

#[test]
fn git_branch_reads_head_ref() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git = dir.path().join(".git");
    std::fs::create_dir_all(&git).expect("mkdir");
    std::fs::write(git.join("HEAD"), "ref: refs/heads/feature-x\n").expect("write");
    assert_eq!(git_branch(dir.path()), Some("feature-x".into()));
    // Ancestor walk: a nested folder resolves to the same repo branch.
    let nested = dir.path().join("a/b");
    std::fs::create_dir_all(&nested).expect("mkdir");
    assert_eq!(git_branch(&nested), Some("feature-x".into()));
}

#[test]
fn git_branch_detached_head_shows_short_hash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git = dir.path().join(".git");
    std::fs::create_dir_all(&git).expect("mkdir");
    std::fs::write(git.join("HEAD"), "0123456789abcdef\n").expect("write");
    assert_eq!(git_branch(dir.path()), Some("@0123456".into()));
}

#[test]
fn git_branch_follows_worktree_gitdir_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (wt, gitdir) = (dir.path().join("wt"), dir.path().join("wt-git"));
    std::fs::create_dir_all(&wt).expect("mkdir");
    std::fs::create_dir_all(&gitdir).expect("mkdir");
    std::fs::write(wt.join(".git"), "gitdir: ../wt-git\n").expect("write");
    std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/wt-branch\n").expect("write");
    assert_eq!(git_branch(&wt), Some("wt-branch".into()));
}

#[test]
fn git_branch_none_outside_repos() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(git_branch(dir.path()), None);
}

// ── /proc plumbing ───────────────────────────────────────────────────────────

#[test]
fn proc_helpers_read_own_process() {
    let pid = std::process::id();
    assert!(proc_comm(pid).is_some_and(|c| !c.is_empty()));
    let ppid = proc_ppid(pid).expect("ppid");
    assert!(ppid > 0);
    assert!(descends_from(pid, ppid));
    assert!(!descends_from(pid, pid)); // a process is not its own ancestor
    assert_eq!(env_var(pid, "PATH"), std::env::var("PATH").ok());
    assert_eq!(env_var(pid, "RICON_TEST_UNSET_VAR"), None);
}

// ── live shells (PTY end-to-end) ─────────────────────────────────────────────

#[test]
fn shell_runs_commands_and_shows_output() {
    // Kata app.md: provides linux terminal/shell functionality.
    let dir = std::env::current_dir().expect("cwd");
    let shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    shell.send(b"echo RICON_$((40+2))\r");
    assert!(
        wait_for(|| screen_contents(&shell).contains("RICON_42"), Duration::from_secs(10)),
        "command output visible, got: {:?}",
        screen_contents(&shell)
    );
    assert_eq!(shell.cwd.as_deref(), Some(dir.as_path()));
}

#[test]
fn pane_drag_selects_text_reverses_it_and_copies() {
    // Kata ui.md: dragging in the pane selects text and copies it (OSC 52).
    let mut app = test_app(&[1]);
    let tok = "RICON_SELECT_ME";
    {
        let shell = app.tabs[0].active_shell_mut();
        assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
        type_and_enter(shell, &format!("echo {tok}"));
        assert!(
            wait_for(|| screen_contents(shell).matches(tok).count() >= 2, Duration::from_secs(10)),
            "echo + output visible, got: {:?}",
            screen_contents(shell)
        );
    }
    // Select the token on the echoed command line — it sits past the prompt,
    // clear of the pane's leftmost column (which is the sidebar-resize handle).
    let (row, col) = {
        let contents = screen_contents(app.tabs[0].active_shell());
        contents
            .lines()
            .enumerate()
            .find_map(|(r, line)| line.find(tok).map(|c| (r as u16, c as u16)))
            .expect("token on screen")
    };
    assert!(col > 1, "token clear of the resize handle, at col {col}");
    let sw = app.sidebar_width;
    let end = col + tok.len() as u16 - 1;
    let ev = |kind, c: u16| MouseEvent { kind, column: sw + c, row, modifiers: KeyModifiers::NONE };
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), col)).expect("press");
    app.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), end)).expect("drag");
    app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), end)).expect("release");

    // Release ends the drag, keeps the selection, and yields the copied text.
    let sel = app.selection.as_ref().expect("selection survives release");
    assert!(!sel.dragging, "release ends the drag");
    assert_eq!(app.selection_text(sel.clone()).as_deref(), Some(tok));
    assert!(app.copied_at.is_some(), "the copy is recorded to fire the footer hint");

    // Rendered: exactly the token's cells carry the selection colors, nothing
    // past it. The highlight is painted, not reversed — reversing cancels out
    // over text that is already inverse.
    let buf = render(&mut app, sw + 80, 25);
    for c in 0..tok.len() as u16 {
        let cell = cell(&buf, sw + col + c, row);
        assert_eq!(cell.bg, SELECT_BG, "cell {c} highlighted");
        assert_eq!(cell.fg, SELECT_FG, "cell {c} legible on the highlight");
    }
    assert_ne!(cell(&buf, sw + col + tok.len() as u16, row).bg, SELECT_BG, "cell past the token untouched");
    // The footer flashes a transient "✓ copied" confirmation right after a copy.
    assert!(row_text(&buf, 24, sw + 80).contains("copied"), "footer hint: {:?}", row_text(&buf, 24, sw + 80));

    // A plain click (press+release, no movement) deselects and copies nothing.
    app.copied_at = None;
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), col)).expect("click");
    app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), col)).expect("release click");
    assert!(app.selection.is_none(), "click deselects");
    assert!(app.copied_at.is_none(), "a no-move click copies nothing");
}

#[test]
fn ctrl_c_is_never_shadowed_and_takes_the_highlight_down() {
    // Kata ui.md: Ctrl+C is always SIGINT — the release already copied, so a
    // live selection only means the highlight is dropped on the way through.
    let mut app = test_app(&[1]);
    let tok = "RICON_CTRL_C_COPY";
    {
        let shell = app.tabs[0].active_shell_mut();
        assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
        type_and_enter(shell, &format!("echo {tok}"));
        assert!(
            wait_for(|| screen_contents(shell).matches(tok).count() >= 2, Duration::from_secs(10)),
            "echo + output visible, got: {:?}",
            screen_contents(shell)
        );
    }
    let (row, col) = {
        let contents = screen_contents(app.tabs[0].active_shell());
        contents
            .lines()
            .enumerate()
            .find_map(|(r, line)| line.find(tok).map(|c| (r as u16, c as u16)))
            .expect("token on screen")
    };
    let sw = app.sidebar_width;
    let ev = |kind, c: u16| MouseEvent { kind, column: sw + c, row, modifiers: KeyModifiers::NONE };
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), col)).expect("press");
    let end = col + tok.len() as u16 - 1;
    app.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), end)).expect("drag");
    app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), end)).expect("release");
    assert!(app.copied_at.is_some(), "the release copied");
    app.copied_at = None;

    // With a selection live, Ctrl+C drops the highlight and still reaches the
    // shell: bash echoes "^C" and re-prompts.
    let sel = app.selection.as_ref().expect("selection");
    assert_eq!(app.selection_text(sel.clone()).as_deref(), Some(tok));
    app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)).expect("ctrl+c");
    assert!(app.copied_at.is_none(), "ctrl+c copies nothing");
    assert!(app.selection.is_none(), "ctrl+c drops the selection");
    assert!(
        wait_for(|| screen_contents(app.tabs[0].active_shell()).contains("^C"), Duration::from_secs(10)),
        "ctrl+c reaches the shell, got: {:?}",
        screen_contents(app.tabs[0].active_shell())
    );
    // Esc does the same: never shadowed, takes a highlight down.
    app.search_focus = false; // Esc on the search row only hands focus back
    app.selection = Some(Selection {
        shell: (0, 0),
        anchor: (row, col),
        head: (row, end),
        dragging: false,
        block: false,
        text: None,
    });
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE)).expect("esc");
    assert!(app.selection.is_none(), "esc drops the selection");
}

#[test]
fn footer_button_shows_and_toggles_copy_mode_and_persists_it() {
    // Kata ui.md: the footer button (flush left of the version) shows copy
    // mode — on by default — and switches it; the choice is persisted at once.
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let mut app = test_app(&[1]);
        assert!(app.copy_mode, "on by default");
        let (w, h) = (app.sidebar_width + 80, 25);
        app.term_width = w;
        let (from, to) = copy_button_x(w).expect("the button fits an 80-column pane");
        let label = |app: &mut App| {
            let buf = render(app, w, h);
            let footer = row_text(&buf, h - 1, w);
            assert!(
                footer.ends_with(concat!("v", env!("CARGO_PKG_VERSION"), " ")),
                "version keeps the corner"
            );
            (
                footer.chars().skip(from as usize).take((to - from) as usize).collect::<String>(),
                cell(&buf, from, h - 1).bg,
            )
        };
        assert_eq!(
            label(&mut app),
            (COPY_ON_BUTTON.to_string(), SELECT_BG),
            "on: the selection color, at its hit-test columns"
        );

        let click = |app: &mut App, column: u16| {
            let ev = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: app.pty_rows,
                modifiers: KeyModifiers::NONE,
            };
            app.on_mouse(ev).expect("footer click");
        };
        click(&mut app, from);
        assert!(!app.copy_mode, "the button switches copy mode off");
        assert!(!load_session().copy_mode, "and persists the choice at once");
        assert_eq!(label(&mut app).0, COPY_OFF_BUTTON, "off reads as off");
        assert!(
            cell(&render(&mut app, w, h), from, h - 1).modifier.contains(Modifier::REVERSED),
            "drawn as a plain button"
        );
        click(&mut app, to - 1);
        assert!(app.copy_mode && load_session().copy_mode, "the last column toggles it back on, persisted");
        // Columns outside the button are not the button.
        click(&mut app, from - 1);
        click(&mut app, to);
        assert!(app.copy_mode, "only the button's own columns toggle the mode");
        // Alt+c is the keyboard's way to the same switch.
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::ALT)).expect("alt+c");
        assert!(!app.copy_mode && !load_session().copy_mode, "alt+c toggles and persists");
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::ALT)).expect("alt+c");
        assert!(app.copy_mode);

        // The "✓ copied" hint lands beside the button, never over it.
        app.flash_copy(true);
        let buf = render(&mut app, w, h);
        let footer = row_text(&buf, h - 1, w);
        let label: String = footer.chars().skip(from as usize).take((to - from) as usize).collect();
        assert!(footer.contains("copied"), "hint shows: {footer:?}");
        assert_eq!(label, COPY_ON_BUTTON, "hint does not cover the button: {footer:?}");
    });
}

#[test]
fn bypass_drag_copies_over_a_mouse_grabbing_app() {
    // Kata ui.md: Alt (or Shift) + drag selects and copies console text even when
    // the inner app has grabbed the mouse (vim/less/htop); a plain drag there
    // forwards. Either bypass selects the pane grid only — never sidebar text.
    let mut app = test_app(&[1]);
    app.copy_mode = false; // with it on, no bypass is needed at all
    let tok = "RICON_BYPASS_COPY";
    {
        let shell = app.tabs[0].active_shell_mut();
        assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
        // Print the token, then turn on mouse reporting exactly as a TUI would.
        type_and_enter(shell, &format!("echo {tok}; printf '\\033[?1000h'"));
        assert!(
            wait_for(|| screen_contents(shell).matches(tok).count() >= 2, Duration::from_secs(10)),
            "token visible: {:?}",
            screen_contents(shell)
        );
    }
    assert!(
        wait_for(
            || app.tabs[0].active_shell().modes().mouse_mode != MouseProtocolMode::None,
            Duration::from_secs(10)
        ),
        "inner app grabbed the mouse"
    );
    // Re-locate the token each time: a forwarded press can append to the prompt
    // line, though never to the output rows the token sits on.
    let find_tok = |app: &App| {
        screen_contents(app.tabs[0].active_shell())
            .lines()
            .enumerate()
            .find_map(|(r, line)| line.find(tok).map(|c| (r as u16, c as u16)))
            .expect("token on screen")
    };
    let sw = app.sidebar_width;
    let at = |kind, c: u16, r: u16, m| MouseEvent { kind, column: sw + c, row: r, modifiers: m };

    // A plain press is forwarded to the app, so no local selection starts.
    let (row, col) = find_tok(&app);
    assert!(col > 1, "token clear of the resize handle, at col {col}");
    app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), col, row, KeyModifiers::NONE)).expect("plain");
    assert!(app.selection.is_none(), "a plain drag over a mouse app does not select");

    // Alt+drag (host terminals keep Shift for themselves) and Shift+drag both
    // select locally and copy on release — the bypass.
    for m in [KeyModifiers::ALT, KeyModifiers::SHIFT] {
        app.copied_at = None;
        let (row, col) = find_tok(&app);
        let end = col + tok.len() as u16 - 1;
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), col, row, m)).expect("press");
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), end, row, m)).expect("drag");
        app.on_mouse(at(MouseEventKind::Up(MouseButton::Left), end, row, m)).expect("release");
        let sel = app.selection.as_ref().expect("bypass drag makes a selection");
        assert_eq!(
            app.selection_text(sel.clone()).as_deref(),
            Some(tok),
            "only the console token is copied ({m:?})"
        );
        assert!(app.copied_at.is_some(), "the copy is recorded ({m:?})");
    }

    // Dragging back over the sidebar clamps to pane column 0: the selection is a
    // pane-grid rectangle, so tab text can never enter it.
    let (row, col) = find_tok(&app);
    let alt = KeyModifiers::ALT;
    app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), col, row, alt)).expect("press");
    let into_sidebar =
        MouseEvent { kind: MouseEventKind::Drag(MouseButton::Left), column: 0, row, modifiers: alt };
    app.on_mouse(into_sidebar).expect("drag into the sidebar");
    let sel = app.selection.as_ref().expect("selection survives the drag");
    assert_eq!(sel.head.1, 0, "head clamps to the pane's first column");
    let text = app.selection_text(sel.clone()).expect("text");
    assert!(!text.contains('⌕') && !text.contains("ricon "), "no sidebar text in the copy: {text:?}");
}

#[test]
fn shift_ctrl_drag_makes_a_block_selection_and_copies_the_rectangle() {
    // Kata ui.md: Shift+Ctrl+drag is a block (rectangular) selection — the way
    // to lift a column of text out of a table or a log. Each row's columns are
    // joined by a newline, so the clipboard holds exactly the rectangle that
    // was highlighted, and the highlight is the rectangle, not whole rows.
    let mut app = test_app(&[1]);
    let tok = "RICON_BLOCK";
    {
        let shell = app.tabs[0].active_shell_mut();
        assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
        // Two lines, each carrying the token at a known column, so the block
        // spans two rows and a fixed column range.
        type_and_enter(shell, &format!("echo {tok}AA; echo {tok}BB"));
        assert!(
            wait_for(|| screen_contents(shell).matches(tok).count() >= 4, Duration::from_secs(10)),
            "echo + output visible, got: {:?}",
            screen_contents(shell)
        );
    }
    let contents = screen_contents(app.tabs[0].active_shell());
    // The two output lines (not the echoed command line) each carry the token
    // at a known column; drag the block between them.
    let rows: Vec<u16> =
        contents.lines().enumerate().filter(|(_, l)| l.contains(tok)).map(|(r, _)| r as u16).collect();
    assert!(rows.len() >= 2, "two output lines carry the token: {rows:?}");
    let (row, row2) = (rows[rows.len() - 2], rows[rows.len() - 1]);
    let col = contents.lines().nth(row as usize).unwrap().find(tok).unwrap() as u16;
    let sw = app.sidebar_width;
    let mods = KeyModifiers::SHIFT.union(KeyModifiers::CONTROL);
    let ev = |kind, c: u16, r: u16| MouseEvent { kind, column: sw + c, row: r, modifiers: mods };
    // Drag from the token's start on the first line to its end on the second.
    let end = col + tok.len() as u16 - 1;
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), col, row)).expect("press");
    app.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), end, row2)).expect("drag");
    app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), end, row2)).expect("release");

    let sel = app.selection.as_ref().expect("block selection survives release");
    assert!(sel.block, "Shift+Ctrl marks the selection as a block");
    assert!(!sel.dragging, "release ends the drag");
    let text = app.selection_text(sel.clone()).expect("block text");
    // The rectangle is the token's columns on each row, joined by a newline —
    // the `AA`/`BB` suffixes past the block's right edge are excluded, which is
    // precisely what a reading-order selection would have included.
    assert_eq!(text, format!("{tok}\n{tok}"), "the rectangle, row by row: {text:?}");
    assert!(app.copied_at.is_some(), "the release copies");

    // Rendered: exactly the rectangle's cells carry the selection colors.
    let buf = render(&mut app, sw + 80, 25);
    for r in row..=row2 {
        for c in col..=end {
            assert_eq!(cell(&buf, sw + c, r).bg, SELECT_BG, "cell ({r},{c}) highlighted");
        }
        // The cell just past the block's right edge is untouched.
        assert_ne!(cell(&buf, sw + end + 1, r).bg, SELECT_BG, "cell past the block untouched");
    }
}

#[test]
fn a_finalized_selection_copies_its_snapshot_not_the_live_screen() {
    // The clipboard must hold exactly what was highlighted. Reading the *live*
    // screen at copy time is what made it disagree: an inner app repainting
    // between the last frame and the copy changes the text, so the user got
    // something they never selected. A finalized selection snapshots the text
    // at release, so a later repaint cannot change the copy.
    let tok = "RICON_SNAPSHOT";
    let (mut app, row, col) = app_with_token(tok);
    let sw = app.sidebar_width;
    let end = col + tok.len() as u16 - 1;
    let ev = |kind, c: u16| MouseEvent { kind, column: sw + c, row, modifiers: KeyModifiers::NONE };
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), col)).expect("press");
    app.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), end)).expect("drag");
    app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), end)).expect("release");
    let sel = app.selection.as_ref().expect("selection");
    assert_eq!(app.selection_text(sel.clone()).as_deref(), Some(tok), "the snapshot is the token");

    // The inner app repaints the very same cells with different text. The copy
    // must still be the token that was highlighted, not the new text.
    app.tabs[0].active_shell().send(b"\r");
    thread::sleep(Duration::from_millis(150));
    let sel = app.selection.as_ref().expect("selection survives the repaint");
    assert_eq!(
        app.selection_text(sel.clone()).as_deref(),
        Some(tok),
        "the copy is the snapshot, not the live screen"
    );
}

/// A live shell with `tok` echoed to the screen, plus the token's grid position
/// — the fixture every selection test starts from.
fn app_with_token(tok: &str) -> (App, u16, u16) {
    let mut app = test_app(&[1]);
    {
        let shell = app.tabs[0].active_shell_mut();
        assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
        type_and_enter(shell, &format!("echo {tok}"));
        assert!(
            wait_for(|| screen_contents(shell).matches(tok).count() >= 2, Duration::from_secs(10)),
            "echo + output visible, got: {:?}",
            screen_contents(shell)
        );
    }
    let contents = screen_contents(app.tabs[0].active_shell());
    let (row, col) = contents
        .lines()
        .enumerate()
        .find_map(|(r, line)| line.find(tok).map(|c| (r as u16, c as u16)))
        .expect("token on screen");
    (app, row, col)
}

#[test]
fn copy_mode_selects_over_an_app_that_grabbed_the_mouse_by_default() {
    // Kata ui.md: copy mode — on by default — is the no-modifier way to lift
    // text out of an app that owns the mouse (a coding agent, vim, less): a
    // plain drag selects locally and the left button never reaches the app,
    // while its wheel and other buttons still do. Switched off, the whole
    // mouse goes to the app.
    let tok = "RICON_COPY_MODE";
    let (mut app, row, col) = app_with_token(tok);
    app.tabs[0].active_shell().send(b"printf '\\033[?1003h\\033[?1006h'\r");
    assert!(
        wait_for(
            || app.tabs[0].active_shell().modes().mouse_mode != MouseProtocolMode::None,
            Duration::from_secs(10)
        ),
        "inner app grabbed the mouse"
    );
    assert!(app.copy_mode, "on by default");

    // A plain drag selects instead of being forwarded: the shell sees no
    // left-button report at all, so its screen is untouched by the gesture.
    let before = screen_contents(app.tabs[0].active_shell());
    let sw = app.sidebar_width;
    let end = col + tok.len() as u16 - 1;
    let ev = |kind, c: u16| MouseEvent { kind, column: sw + c, row, modifiers: KeyModifiers::NONE };
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), col)).expect("press");
    app.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), end)).expect("drag");
    app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), end)).expect("release");
    let sel = app.selection.as_ref().expect("copy mode makes a selection");
    assert_eq!(app.selection_text(sel.clone()).as_deref(), Some(tok), "only the console token");
    assert!(app.copied_at.is_some(), "the release copies");
    assert!(app.copy_mode, "the mode stays on: it is a setting, not a one-shot");
    // A middle click is ricon's too (it pastes) — nothing reaches the app.
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Middle), col)).expect("middle");
    app.on_mouse(ev(MouseEventKind::Up(MouseButton::Middle), col)).expect("middle up");
    thread::sleep(Duration::from_millis(150));
    assert_eq!(screen_contents(app.tabs[0].active_shell()), before, "no mouse report reached the app");
    // The wheel is not taken: an app that asked for the mouse scrolls itself.
    app.on_mouse(ev(MouseEventKind::ScrollDown, col)).expect("wheel");
    assert!(
        wait_for(|| screen_contents(app.tabs[0].active_shell()) != before, Duration::from_secs(10)),
        "the wheel report reached the app"
    );

    // Off, a plain press is the app's again.
    app.copy_mode = false;
    app.selection = None;
    let before = screen_contents(app.tabs[0].active_shell());
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), col)).expect("press");
    assert!(app.selection.is_none(), "no selection with the mode off");
    assert!(
        wait_for(|| screen_contents(app.tabs[0].active_shell()) != before, Duration::from_secs(10)),
        "the press was forwarded to the app"
    );
}

#[test]
fn double_click_takes_the_word_and_triple_click_the_line() {
    // Kata ui.md: the gestures every terminal has. Hosts only report single
    // presses, so ricon reconstructs the chain from timing.
    let tok = "RICON_WORD_/tmp/a-b.txt_END";
    let (mut app, row, col) = app_with_token(tok);
    let sw = app.sidebar_width;
    let press = |app: &mut App, c: u16| {
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: sw + c,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(ev).expect("press");
    };
    // Two presses on the same cell: the whole token is one word (a path keeps
    // its slashes, dots and dashes).
    press(&mut app, col + 3);
    press(&mut app, col + 3);
    let sel = app.selection.as_ref().expect("double click selects");
    assert!(!sel.dragging, "a word selection is complete, not a live drag");
    assert_eq!(app.selection_text(sel.clone()).as_deref(), Some(tok), "the word under the cursor");
    assert!(app.copied_at.is_some(), "a double click copies straight away");

    // A third press takes the whole line, which still holds the token.
    app.copied_at = None;
    press(&mut app, col + 3);
    let sel = app.selection.as_ref().expect("triple click selects");
    assert_eq!(sel.anchor, (row, 0), "the line starts at column 0");
    assert_eq!(sel.head, (row, app.pty_cols - 1), "and runs to the last column");
    let line = app.selection_text(sel.clone()).expect("line text");
    assert!(line.contains(tok), "the whole line: {line:?}");
    assert!(app.copied_at.is_some(), "a triple click copies too");

    // A press after the chain window starts over as a plain drag.
    app.last_click = None;
    press(&mut app, col + 3);
    assert!(app.selection.as_ref().expect("fresh press").dragging, "a lone click starts a drag selection");
}

#[test]
fn alt_a_selects_the_whole_screen_and_copies_it() {
    // Kata ui.md: "copy everything" stays reachable, but it is drawn as a
    // selection first — the button never takes the screen behind the user's back.
    let tok = "RICON_SELECT_ALL";
    let (mut app, _, _) = app_with_token(tok);
    app.on_key(key(KeyCode::Char('a'), KeyModifiers::ALT)).expect("alt+a");
    let sel = app.selection.as_ref().expect("alt+a selects");
    assert_eq!(sel.anchor, (0, 0));
    assert_eq!(sel.head, (app.pty_rows - 1, app.pty_cols - 1));
    assert!(
        app.selection_text(sel.clone()).expect("text").contains(tok),
        "the visible screen is the selection"
    );
    assert!(app.copied_at.is_some(), "and it is copied");
}

#[test]
fn scrolling_carries_the_selection_with_the_text() {
    // Kata ui.md: the wheel scrolls the pane and the selection rides along with
    // the lines it covers, so a selection can span more than one screenful.
    let tok = "RICON_SCROLL_SEL";
    let (mut app, row, col) = app_with_token(tok);
    let sw = app.sidebar_width;
    app.selection = Some(Selection {
        shell: (0, 0),
        anchor: (row, col),
        head: (row, col + 3),
        dragging: false,
        block: false,
        text: None,
    });
    // Fill the scrollback so there is somewhere to scroll to.
    app.tabs[0].active_shell().send(b"seq 1 200\r");
    // The echoed command already reads `seq 1 200`, so waiting for that text
    // can pass before a single line of output exists and leave the scroll with
    // nothing to move. Wait for the output's own last line instead.
    assert!(
        wait_for(
            || screen_contents(app.tabs[0].active_shell()).lines().any(|l| l.trim() == "200"),
            Duration::from_secs(10)
        ),
        "scrollback filled"
    );
    let wheel = |kind| MouseEvent { kind, column: sw + 1, row: 1, modifiers: KeyModifiers::NONE };
    app.on_mouse(wheel(MouseEventKind::ScrollUp)).expect("wheel up");
    let sel = app.selection.as_ref().expect("the selection survives a scroll");
    assert_eq!(sel.anchor.0, row + SCROLL_STEP as u16, "it moved down with the text");
    assert_eq!(sel.anchor.1, col, "columns are untouched");
    // Scrolling it clean off the grid drops it rather than leaving a lie on screen.
    for _ in 0..app.pty_rows {
        app.on_mouse(wheel(MouseEventKind::ScrollUp)).expect("wheel up");
    }
    assert!(app.selection.is_none(), "a selection scrolled out of view is dropped");
}

#[test]
fn selection_is_dropped_when_another_shell_takes_the_screen() {
    // A selection belongs to the shell it was made in: switching tab or shell
    // must drop it, so the copy button can never reach for invisible text.
    let mut app = test_app(&[1, 1]);
    app.selection = Some(Selection {
        shell: (0, 0),
        anchor: (1, 1),
        head: (1, 5),
        dragging: false,
        block: false,
        text: None,
    });
    app.drop_stale_selection();
    assert!(app.selection.is_some(), "kept while its own shell is on screen");
    app.active = 1;
    app.drop_stale_selection();
    assert!(app.selection.is_none(), "dropped once another tab is shown");
}

#[test]
fn input_never_blocks_the_ui_when_the_inner_app_stops_reading() {
    // A PTY write blocks once the tty buffer fills and nothing is reading it —
    // a big paste into a busy program. The writer thread absorbs that, so the
    // render loop is never the thing waiting.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    shell.send(b"sleep 30\r"); // foreground program that reads nothing
    assert!(
        wait_for(
            || {
                shell.sample_proc();
                shell.process == "sleep"
            },
            Duration::from_secs(10)
        ),
        "program in foreground"
    );
    let paste = vec![b'x'; 512 * 1024]; // far past any tty buffer
    let start = Instant::now();
    shell.send(&paste);
    assert!(start.elapsed() < Duration::from_millis(200), "send returned at once: {:?}", start.elapsed());
}

#[test]
fn shell_reports_foreground_process_and_cmdline() {
    // Kata app.md: third row shows the running process; commands persist.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    thread::sleep(Duration::from_millis(200)); // let the prompt settle
    shell.send(b"sleep 30\r");
    assert!(
        wait_for(
            || {
                shell.sample_proc();
                shell.process == "sleep"
            },
            Duration::from_secs(10)
        ),
        "foreground process detected, got {:?}",
        shell.process
    );
    // The same single /proc pass cached the full command line (what the
    // session persists) — no extra IO at persist time.
    assert_eq!(shell.fg_cmd.as_deref(), Some("sleep 30"));
}

#[test]
fn shell_replays_pending_command_after_prompt() {
    // Kata app.md: persisted commands are restored when the app restarts.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = Shell::spawn(24, 80, &dir, Some("echo RICON_RESTORED".into())).expect("spawn");
    assert!(
        wait_for(
            || {
                shell.flush_pending();
                screen_contents(&shell).contains("RICON_RESTORED")
            },
            Duration::from_secs(10)
        ),
        "pending command replayed"
    );
    assert!(shell.pending_cmd.is_none());
}

#[test]
fn shell_resize_updates_screen_and_starts_grace() {
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    shell.resize(30, 100);
    let size = shell.parser.lock().unwrap_or_else(PoisonError::into_inner).screen().size();
    assert_eq!(size, (30, 100));
    assert!(shell.resized.elapsed() < RESIZE_GRACE);
}

#[test]
fn tick_activity_flags_unseen_output_and_animates() {
    // Kata app.md: `*` for off-screen output; spinner runs 1 s past settle.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    // Quiescent baseline: everything settled long ago.
    shell.seen_activity = shell.activity.load(Ordering::Relaxed);
    shell.resized = Instant::now() - RESIZE_GRACE * 2;
    shell.last_change = Instant::now() - SETTLE * 2;
    shell.tick_activity(false);
    assert!(!shell.animating && !shell.unseen_output);
    // New output while the shell is off screen → marker + animation.
    shell.activity.fetch_add(1, Ordering::Relaxed);
    shell.tick_activity(false);
    assert!(shell.animating && shell.unseen_output);
    // Focusing the shell clears the marker but not the running animation.
    shell.tick_activity(true);
    assert!(shell.animating && !shell.unseen_output);
}

#[test]
fn tick_activity_ignores_output_within_resize_grace() {
    // Kata ui.md: tab bar resize triggers neither `*` nor the animation.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    shell.seen_activity = shell.activity.load(Ordering::Relaxed);
    shell.last_change = Instant::now() - SETTLE * 2;
    shell.resized = Instant::now(); // a resize just happened
    shell.activity.fetch_add(1, Ordering::Relaxed); // SIGWINCH repaint
    shell.tick_activity(false);
    assert!(!shell.animating && !shell.unseen_output);
}

#[test]
fn scroll_moves_view_and_input_snaps_back() {
    // Kata app.md: terminal output is scrollable.
    let mut app = test_app(&[1]);
    {
        let shell = app.tabs[0].active_shell();
        let mut parser = shell.parser.lock().unwrap_or_else(PoisonError::into_inner);
        for i in 0..100 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
    }
    app.tabs[0].active_shell().scroll(10);
    let scrollback = {
        let parser = app.tabs[0].active_shell().parser.lock().unwrap_or_else(PoisonError::into_inner);
        parser.screen().scrollback()
    };
    assert!(scrollback > 0, "wheel scrolled into history");
    app.write_active(b""); // any input snaps back to live
    let parser = app.tabs[0].active_shell().parser.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(parser.screen().scrollback(), 0);
}

// ── tabs & shells (app behavior) ─────────────────────────────────────────────

#[test]
fn tab_rows_grow_two_per_subshell() {
    // Kata app.md: 4 rows per tab, plus two rows per subshell.
    let mut app = test_app(&[1, 3]);
    assert_eq!(app.tabs[0].rows(), 4);
    assert_eq!(app.tabs[1].rows(), 8);
    assert_eq!(app.content_rows(), 12);
    // Kata app.md: Alt+Up/Down cycles shells within the tab, wrapping.
    let tab = &mut app.tabs[1];
    assert_eq!(tab.active, 0);
    tab.navigate(-1);
    assert_eq!(tab.active, 2);
    tab.navigate(1);
    assert_eq!(tab.active, 0);
}

#[test]
fn open_tab_lands_after_active_but_below_favorites() {
    // Kata app.md: new tab opens right after the active tab or after the last
    // favorite tab, whichever comes later.
    let mut app = test_app(&[1, 1]);
    let (c0, c1) = (app.tabs[0].shells[0].pid, app.tabs[1].shells[0].pid);
    app.toggle_favorite(); // favorite tab 0 (stays at index 0)
    app.open_tab().expect("open");
    assert_eq!(app.tabs.len(), 3);
    assert_eq!(app.active, 1, "new tab lands after the favorites block");
    assert_eq!(app.tabs[0].shells[0].pid, c0);
    assert_eq!(app.tabs[2].shells[0].pid, c1);
    // The new tab inherits the active tab's directory (the base here).
    assert_eq!(app.tabs[1].shells[0].cwd.as_deref(), Some(app.base.as_path()));
    assert!(app.tabs[1].shells[0].pid != c0 && app.tabs[1].shells[0].pid != c1, "a new shell of its own");
}

#[test]
fn open_subshell_joins_active_tab_and_focuses() {
    // Kata app.md: Alt+s adds a shell to the active tab; it shares the color.
    let mut app = test_app(&[1]);
    app.open_subshell().expect("subshell");
    assert_eq!(app.tabs.len(), 1, "subshells never create tabs");
    assert_eq!(app.tabs[0].shells.len(), 2);
    assert_eq!(app.tabs[0].active, 1, "new subshell is focused");
}

#[test]
fn toggle_favorite_clusters_at_top_in_marking_order() {
    // Kata app.md: favorites go on top, after the last existing favorite.
    let mut app = test_app(&[1, 1, 1]);
    let (a, b, c) = (app.tabs[0].shells[0].pid, app.tabs[1].shells[0].pid, app.tabs[2].shells[0].pid);
    app.active = 2;
    app.toggle_favorite(); // C → favorite, moves to top
    assert_eq!((app.tabs[0].shells[0].pid, app.active), (c, 0));
    app.active = 2;
    app.toggle_favorite(); // B → favorite, lands after C
    assert_eq!(app.tabs[1].shells[0].pid, b);
    assert_eq!(app.tabs[2].shells[0].pid, a);
    assert!(app.tabs[0].favorite && app.tabs[1].favorite && !app.tabs[2].favorite);
    app.toggle_favorite(); // unmark B → drops just below the favorites block
    assert!(!app.tabs[1].favorite);
    assert_eq!(app.tabs[1].shells[0].pid, b);
}

#[test]
fn close_active_kills_only_the_active_shell() {
    // Kata app.md: with multiple shells, Ctrl+w closes only the active shell.
    let mut app = test_app(&[2, 1]);
    app.tabs[0].active = 1;
    app.close_active();
    assert!(
        wait_for(
            || {
                app.reap_dead_tabs();
                app.tabs[0].shells.len() == 1
            },
            Duration::from_secs(10)
        ),
        "subshell reaped"
    );
    assert_eq!(app.tabs.len(), 2, "tab survives its subshell");
    assert_eq!(app.tabs[0].active, 0);
    // Closing a tab's last shell closes the tab itself.
    app.active = 1;
    app.close_active();
    assert!(
        wait_for(
            || {
                app.reap_dead_tabs();
                app.tabs.len() == 1
            },
            Duration::from_secs(10)
        ),
        "tab reaped"
    );
    assert_eq!(app.active, 0);
}

#[test]
fn closing_a_tab_refreshes_shown_so_tab_switching_cannot_panic() {
    // A whole burst of events is drained between two frames. Closing a tab
    // reaps it and shrinks `tabs`, so `shown` (indices into `tabs`) must be
    // refreshed at once — otherwise a tab-switch right after the close resolves
    // its row against the old order, lands `active` on a removed index, and the
    // next pane click indexes out of bounds.
    let mut app = test_app(&[1, 1, 1]);
    app.active = 1;
    app.close_active();
    assert!(
        wait_for(
            || {
                app.reap_dead_tabs();
                app.tabs.len() == 2
            },
            Duration::from_secs(10)
        ),
        "tab reaped"
    );
    // `shown` is refreshed by the reap, so navigating cannot point at a dead tab.
    assert_eq!(app.shown, vec![0, 1], "shown tracks the surviving tabs");
    app.navigate_tabs(1);
    assert!(app.active < app.tabs.len(), "active stays in bounds after a close+switch");
    // And a pane click on the active tab does not panic.
    let sw = app.sidebar_width;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: sw + 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(click).expect("pane click after close+switch");
}

// ── sidebar layout math ──────────────────────────────────────────────────────

#[test]
fn tab_at_row_walks_variable_heights() {
    let mut app = test_app(&[1, 2]); // heights 4 and 6
    assert_eq!(app.tab_at_row(0), None, "title row");
    assert_eq!(app.tab_at_row(1), None, "search row");
    assert_eq!(app.tab_at_row(2), Some(0));
    assert_eq!(app.tab_at_row(5), Some(0), "separator belongs to its tab");
    assert_eq!(app.tab_at_row(6), Some(1));
    assert_eq!(app.tab_at_row(11), Some(1));
    assert_eq!(app.tab_at_row(12), None, "past the last tab");
    app.list_offset = 1; // scrolled: the first tab row is now tab 1
    assert_eq!(app.tab_at_row(2), Some(1));
}

#[test]
fn shell_at_row_resolves_subshells() {
    // Kata app.md: a click selects the specific shell whose row was hit.
    let app = test_app(&[1, 2]);
    assert_eq!(app.shell_at_row(0), None);
    assert_eq!(app.shell_at_row(1), None, "search row");
    assert_eq!(app.shell_at_row(2), Some((0, 0)), "name row → parent");
    assert_eq!(app.shell_at_row(4), Some((0, 0)), "process row → parent");
    assert_eq!(app.shell_at_row(5), Some((0, 0)), "separator → parent");
    assert_eq!(app.shell_at_row(6), Some((1, 0)), "tab 1 name row");
    assert_eq!(app.shell_at_row(9), Some((1, 1)), "subshell path row");
    assert_eq!(app.shell_at_row(10), Some((1, 1)), "subshell process row");
    assert_eq!(app.shell_at_row(11), Some((1, 0)), "tab 1 separator → parent");
}

#[test]
fn scrolling_clamps_and_reveals_active() {
    // Kata app.md: tab list is scrollable when it exceeds the available area.
    let mut app = test_app(&[1, 1, 1]); // 12 content rows
    app.sidebar_rows = 10; // 8 viewport rows (title + search + 8)
    assert!(app.tabs_overflow());
    assert_eq!(app.max_offset(), 1);
    app.scroll_tabs(10);
    assert_eq!(app.list_offset, 1, "clamped to max offset");
    app.scroll_tabs(-10);
    assert_eq!(app.list_offset, 0, "clamped to top");
    // Activating an off-screen tab scrolls it into view minimally.
    app.active = 2;
    app.reveal_active();
    assert_eq!(app.list_offset, 1);
    app.active = 0;
    app.reveal_active();
    assert_eq!(app.list_offset, 0);
}

#[test]
fn fit_ptys_resizes_shells_and_reclamps_sidebar() {
    let mut app = test_app(&[1]);
    app.fit_ptys(Rect::new(0, 0, 120, 40));
    assert_eq!((app.pty_rows, app.pty_cols), (39, 120 - SIDEBAR_WIDTH));
    let size = {
        let parser = app.tabs[0].active_shell().parser.lock().unwrap_or_else(PoisonError::into_inner);
        parser.screen().size()
    };
    assert_eq!(size, (39, 120 - SIDEBAR_WIDTH));
    // A dragged-wide sidebar re-clamps when the terminal shrinks under it.
    app.sidebar_width = 100;
    app.fit_ptys(Rect::new(0, 0, 60, 40));
    assert_eq!(app.sidebar_width, 60 - MIN_PANE_WIDTH);
    // Degenerate sizes leave the PTY untouched (vt100 can't go that small).
    let before = (app.pty_rows, app.pty_cols);
    app.fit_ptys(Rect::new(0, 0, 60, 2));
    assert_eq!((app.pty_rows, app.pty_cols), before);
}

// ── UI rendering (TestBackend) ───────────────────────────────────────────────

const W: u16 = 60;
const H: u16 = 20;

#[test]
fn render_active_tab_shows_gutter_marker_and_number() {
    // Kata app.md: active tab shows ▶; tab states terminal number starting at
    // 1; active tab lines carry `│` as the first character.
    let mut app = test_app(&[1, 1]);
    let buf = render(&mut app, W, H);
    assert!(row_text(&buf, 0, W).contains(" ricon "), "sidebar title");
    assert!(row_text(&buf, 1, W).contains('⌕'), "search row sits before all tabs");
    let first = row_text(&buf, 2, W);
    assert!(first.starts_with("│▶1"), "active tab row: {first:?}");
    let folder = folder_name(&app.base);
    assert!(first.contains(&folder), "tab name is the folder name");
    // Rows 2–5 (all lines of the active tab, empty separator included) carry │.
    for y in 2..=5 {
        assert_eq!(cell(&buf, 0, y).symbol(), "│", "gutter on row {y}");
    }
    // The inactive tab has neither gutter nor marker, but keeps its number.
    let second = row_text(&buf, 6, W);
    assert!(second.starts_with("  2"), "inactive tab row: {second:?}");
    assert_eq!(cell(&buf, 0, 9).symbol(), " ", "no gutter on inactive separator");
}

#[test]
fn render_second_row_is_full_path_third_is_process() {
    // Kata app.md: second tab row = full path, third row = process name.
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].process = "bash".into();
    // `$HOME` decides how the path is drawn and other tests swap it, so the
    // render and the expectation must observe the same value.
    let (buf, expected) = with_env_lock(|| {
        let buf = render(&mut app, W, H);
        (buf, abbreviate_home(&app.base.display().to_string()))
    });
    let path_row = row_text(&buf, 3, W);
    let shown = truncate_tail(&expected, SIDEBAR_WIDTH, 6);
    assert!(path_row.contains(&shown), "path row {path_row:?} shows {shown:?}");
    let process_row = row_text(&buf, 4, W);
    assert!(process_row.contains("└ bash"), "process row: {process_row:?}");
}

#[test]
fn render_favorite_star_and_unseen_marker() {
    // Kata app.md: ⭐ before the name of favorites; `*` after the name when an
    // off-screen shell produced output.
    let mut app = test_app(&[1, 1]);
    app.tabs[0].favorite = true;
    app.tabs[1].shells[0].unseen_output = true;
    let buf = render(&mut app, W, H);
    let first = row_text(&buf, 2, W);
    assert!(first.contains("⭐"), "favorite star: {first:?}");
    let star_x = (0..W).find(|&x| cell(&buf, x, 2).symbol() == "⭐").expect("star cell");
    assert_eq!(cell(&buf, star_x, 2).style().fg, Some(Color::Yellow));
    // The `*` sits after the name and before the tab's auto button, which is
    // glued to the panel's right edge (kata ai.md).
    let second = row_text(&buf, 6, SIDEBAR_WIDTH - 1);
    let (star, button) = (second.find('*'), second.find("⟳ auto"));
    assert!(star < button && button.is_some(), "unseen marker before the auto button: {second:?}");
}

#[test]
fn render_active_shell_marker_and_bold_in_multishell_tab() {
    // Kata app.md: active shell shows ▶ with two prefix spaces and its rows
    // are bold; the parent's rows are not when a subshell is active.
    let mut app = test_app(&[2]);
    app.tabs[0].active = 1;
    let buf = render(&mut app, W, H);
    let sub_path = row_text(&buf, 5, W);
    assert!(sub_path.starts_with("│  ▶"), "subshell marker: {sub_path:?}");
    assert!(cell(&buf, 3, 5).style().add_modifier.contains(Modifier::BOLD), "active shell bold");
    let parent_path = row_text(&buf, 3, W);
    assert!(!parent_path.contains('▶'), "parent not marked: {parent_path:?}");
    assert!(!cell(&buf, 3, 3).style().add_modifier.contains(Modifier::BOLD), "inactive shell not bold");
}

#[test]
fn render_spinner_is_braille_white_on_activity() {
    // Kata app.md: white braille spinner after the process while active — on
    // the active tab's pastel, where white would vanish, in its dark text color.
    let mut app = test_app(&[1, 1]);
    let braille = |buf: &Buffer, y: u16| {
        (0..W).find(|&x| matches!(cell(buf, x, y).symbol().chars().next(), Some('\u{2800}'..='\u{28FF}')))
    };
    for tab in 0..2 {
        app.tabs[tab].shells[0].process = "cargo".into();
        app.tabs[tab].shells[0].animating = true;
    }
    let buf = render(&mut app, W, H);
    let (active_row, inactive_row) = (4, 8);
    let x = braille(&buf, active_row).expect("spinner on the active tab's process row");
    let style = cell(&buf, x, active_row).style();
    assert_eq!(style.fg, Some(ACTIVE_FG), "dark on the pastel");
    assert!(style.add_modifier.contains(Modifier::BOLD));
    let x = braille(&buf, inactive_row).expect("spinner on the inactive tab's process row");
    let style = cell(&buf, x, inactive_row).style();
    assert_eq!(style.fg, Some(SPINNER_COLOR), "white on an inactive tab");
    assert!(style.add_modifier.contains(Modifier::BOLD));
    // Without activity the spinner disappears.
    app.tabs[0].shells[0].animating = false;
    let buf = render(&mut app, W, H);
    assert!(braille(&buf, active_row).is_none(), "spinner removed after settle");
}

#[test]
fn render_paints_only_the_active_tab_in_the_pastel() {
    // Kata app.md: the active tab (every one of its rows) and the footer wear
    // the one pastel with dark text; inactive tabs paint no background at all.
    let mut app = test_app(&[1, 1, 1]);
    let painted = |buf: &Buffer, y: u16| (cell(buf, 1, y).bg, cell(buf, 1, y).fg);
    let buf = render(&mut app, W, H);
    for y in 2..5 {
        assert_eq!(painted(&buf, y), (ACTIVE_BG, ACTIVE_FG), "active tab row {y}");
    }
    assert_eq!(cell(&buf, 1, 5).bg, ACTIVE_BG, "the activity row wears the pastel too");
    for y in 6..14 {
        assert_eq!(cell(&buf, 1, y).bg, Color::Reset, "inactive tab row {y} has no background");
        assert_eq!(cell(&buf, 1, y).fg, Color::Reset, "inactive tab row {y} takes the terminal's text color");
    }
    assert_eq!(painted(&buf, H - 1), (ACTIVE_BG, ACTIVE_FG), "footer wears the pastel");
    app.active = 1;
    let buf = render(&mut app, W, H);
    assert_eq!(cell(&buf, 1, 2).bg, Color::Reset, "tab 0 lost the pastel");
    assert_eq!(painted(&buf, 6), (ACTIVE_BG, ACTIVE_FG), "tab 1 took it");
}

#[test]
fn render_activity_bar_sweeps_on_the_active_tabs_last_row() {
    // Kata app.md: the active tab's last row is a bar — a track across the
    // panel, with a lit segment sweeping along it while output streams.
    let mut app = test_app(&[1, 1]);
    let sw = app.sidebar_width;
    // The row inside the panel — its border column excluded.
    let last = |buf: &Buffer, y: u16| row_text(buf, y, sw - 1);
    app.tabs[0].shells[0].animating = false;
    let buf = render(&mut app, W, H);
    let quiet = last(&buf, 5);
    assert!(quiet.starts_with('│'), "the gutter stays: {quiet:?}");
    assert_eq!(
        quiet.chars().filter(|&c| c == '─').count(),
        (sw - 2) as usize,
        "the track spans the panel: {quiet:?}"
    );
    assert!(!quiet.contains('━'), "nothing lit while quiet: {quiet:?}");
    assert_eq!(cell(&buf, 1, 5).fg, BAR_TRACK, "the track in its quiet color");
    assert_eq!(last(&buf, 9).trim(), "", "an inactive tab's last row stays empty");
    app.tabs[0].shells[0].animating = true;
    let buf = render(&mut app, W, H);
    let busy = last(&buf, 5);
    let lit = busy.chars().filter(|&c| c == '━').count();
    assert!((1..=BAR_LEN).contains(&lit), "a segment is lit: {busy:?}");
    let x = busy.chars().position(|c| c == '━').expect("lit cell") as u16;
    assert_eq!((cell(&buf, x, 5).fg, cell(&buf, x, 5).bg), (BAR_LIT, ACTIVE_BG), "lit on the pastel");
    assert!(cell(&buf, x, 5).modifier.contains(Modifier::BOLD));
}

#[test]
fn render_footer_shows_index_path_branch_and_version() {
    // Kata app.md/meta.md: footer shows `index/count`, path, git branch, and
    // the version pinned to the right corner.
    let mut app = test_app(&[1, 1]);
    app.active = 1;
    // Same reason as the path row: hold `$HOME` still across render + expectation.
    let (buf, path) = with_env_lock(|| {
        let buf = render(&mut app, W, H);
        (buf, abbreviate_home(&app.base.display().to_string()))
    });
    let footer = row_text(&buf, H - 1, W);
    assert!(footer.contains("2/2"), "index/count: {footer:?}");
    assert!(footer.contains(path.trim_start_matches('~')), "path: {footer:?}");
    if let Some(branch) = git_branch(&app.base) {
        assert!(footer.contains(&format!("⎇ {branch}")), "branch: {footer:?}");
    }
    assert!(footer.ends_with(concat!("v", env!("CARGO_PKG_VERSION"), " ")), "version: {footer:?}");
    assert!(cell(&buf, 1, H - 1).style().add_modifier.contains(Modifier::BOLD));
    assert!(!footer.contains('↕'), "no scroll indicator when all tabs fit");
}

#[test]
fn render_footer_scroll_indicator_when_tabs_overflow() {
    // Kata app.md: ↕ before the active tab index when not all tabs fit.
    let mut app = test_app(&[1, 1, 1, 1]);
    let buf = render(&mut app, W, 12); // 16 content rows > 10 viewport rows
    let footer = row_text(&buf, 11, W);
    assert!(footer.contains("↕ 1/4"), "scroll indicator: {footer:?}");
}

#[test]
fn render_honours_scroll_offset() {
    // Kata app.md: mouse wheel scrolls the tab list when it overflows.
    let mut app = test_app(&[1, 1, 1, 1]);
    render(&mut app, W, 12); // first render records sidebar_rows
    app.scroll_tabs(1);
    let buf = render(&mut app, W, 12);
    let first = row_text(&buf, 2, W);
    assert!(first.contains('2'), "tab 2 is first after scrolling: {first:?}");
}

#[test]
fn render_truncates_long_paths_in_narrow_sidebar() {
    let mut app = test_app(&[1]);
    let long = PathBuf::from("/very/long/path/that/cannot/possibly/fit/in/the/sidebar");
    app.tabs[0].shells[0].cwd = Some(long);
    app.sidebar_width = 12;
    let buf = render(&mut app, W, H);
    assert!(row_text(&buf, 3, W).contains('…'), "elided path: {:?}", row_text(&buf, 3, W));
}

#[test]
fn render_quit_dialog_centered_with_no_preselected() {
    // Kata app.md: Ctrl+q shows a centered YES/NO confirmation, NO highlighted.
    let mut app = test_app(&[1]);
    app.confirm_quit = Some(false);
    let buf = render(&mut app, W, H);
    let question =
        (0..H).map(|y| row_text(&buf, y, W)).find(|r| r.contains("Are you sure to Quit all tabs?"));
    assert!(question.is_some(), "question is on screen");
    let y = (0..H).find(|&y| row_text(&buf, y, W).contains("YES")).expect("buttons row");
    let row = row_text(&buf, y, W);
    let yes_x = row.find("YES").expect("YES") as u16;
    let no_x = row.find(" NO ").expect("NO") as u16 + 1;
    assert!(cell(&buf, no_x, y).modifier.contains(Modifier::REVERSED), "NO preselected: {row:?}");
    assert!(!cell(&buf, yes_x, y).modifier.contains(Modifier::REVERSED), "YES not selected: {row:?}");

    app.confirm_quit = Some(true);
    let buf = render(&mut app, W, H);
    assert!(cell(&buf, yes_x, y).modifier.contains(Modifier::REVERSED), "arrow highlights YES");

    app.confirm_quit = None;
    let buf = render(&mut app, W, H);
    assert!(!(0..H).any(|y| row_text(&buf, y, W).contains("Are you sure")), "dialog gone when closed");
}

#[test]
fn status_bar_truncates_left_but_pins_version_right() {
    let app = test_app(&[1]);
    let shell = app.tabs[0].active_shell();
    let footer = |copy_mode| Footer {
        index: 0,
        count: 9,
        shell,
        branch: Some("main".into()),
        width: 30,
        tabs_overflow: true,
        copy_mode,
        auto: true,
    };
    let text = line_text(&status_bar(footer(true)));
    assert_eq!(text.chars().count(), 30, "line exactly fills the width");
    assert!(text.starts_with(" ↕ 1/9"), "indicator and index: {text:?}");
    assert!(text.ends_with(concat!("v", env!("CARGO_PKG_VERSION"), " ")), "version survives: {text:?}");
    // The button reads the mode.
    assert!(text.contains(COPY_ON_BUTTON), "copy mode on: {text:?}");
    assert!(line_text(&status_bar(footer(false))).contains(COPY_OFF_BUTTON), "copy mode off");
}

// ── input dispatch (shortcut wiring) ─────────────────────────────────────────

#[test]
fn shortcuts_drive_tab_selection_and_quit() {
    // Kata app.md: alt+number selects, alt+PgUp/PgDn cycle, Ctrl+q quits.
    let mut app = test_app(&[1, 1, 1]);
    app.on_key(key(KeyCode::Char('2'), KeyModifiers::ALT)).expect("alt+2");
    assert_eq!(app.active, 1);
    app.on_key(key(KeyCode::Char('9'), KeyModifiers::ALT)).expect("alt+9");
    assert_eq!(app.active, 1, "out-of-range tab number ignored");
    app.on_key(key(KeyCode::PageDown, KeyModifiers::ALT)).expect("alt+pgdn");
    assert_eq!(app.active, 2);
    app.on_key(key(KeyCode::PageDown, KeyModifiers::ALT)).expect("alt+pgdn");
    assert_eq!(app.active, 0, "next wraps");
    app.on_key(key(KeyCode::PageUp, KeyModifiers::ALT)).expect("alt+pgup");
    assert_eq!(app.active, 2, "previous wraps");
    app.on_key(key(KeyCode::Char('q'), KeyModifiers::ALT)).expect("alt+q");
    assert!(!app.quit, "Alt+q alone no longer quits");
    assert_eq!(app.confirm_quit, Some(false), "Alt+q opens the dialog with NO preselected");
}

#[test]
fn alt_shortcuts_open_and_close_tabs_focus_search_and_mark_favorites() {
    // Kata app.md: every shortcut of ricon's own is on Alt — Alt+t/Alt+n open a
    // tab, Alt+w closes the active shell, Alt+f focuses the search row,
    // Alt+Shift+f marks a favorite; the Ctrl keys they used to shadow reach
    // the shell.
    let mut app = test_app(&[1]);
    app.search_focus = false;
    app.on_key(key(KeyCode::Char('t'), KeyModifiers::ALT)).expect("alt+t");
    app.on_key(key(KeyCode::Char('n'), KeyModifiers::ALT)).expect("alt+n");
    assert_eq!(app.tabs.len(), 3, "alt+t and alt+n each open a tab");
    assert_eq!(app.active, 2);
    app.on_key(key(KeyCode::Char('w'), KeyModifiers::ALT)).expect("alt+w");
    assert!(
        wait_for(
            || {
                app.reap_dead_tabs();
                app.tabs.len() == 2
            },
            Duration::from_secs(10)
        ),
        "alt+w closes it"
    );
    app.on_key(key(KeyCode::Char('F'), KeyModifiers::ALT | KeyModifiers::SHIFT)).expect("alt+shift+f");
    assert!(app.tabs[0].favorite, "alt+shift+f marks the favorite");
    assert!(!app.search_focus, "and does not touch the search row");
    app.on_key(key(KeyCode::Char('f'), KeyModifiers::ALT)).expect("alt+f");
    assert!(app.search_focus, "alt+f focuses the search row");
    app.on_key(key(KeyCode::Char('f'), KeyModifiers::ALT)).expect("alt+f again");
    assert!(app.tabs[0].favorite, "alt+f never toggles the favorite");
    // Ctrl+t reaches the shell now (bash swaps the two characters before the cursor).
    app.search_focus = false;
    let shell = app.tabs[0].active_shell();
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    app.write_active(b"ab");
    app.on_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL)).expect("ctrl+t");
    assert!(
        wait_for(|| screen_contents(app.tabs[0].active_shell()).contains("ba"), Duration::from_secs(10)),
        "ctrl+t is bash's transpose, not a new tab: {:?}",
        screen_contents(app.tabs[0].active_shell())
    );
    assert_eq!(app.tabs.len(), 2, "no tab opened on ctrl+t");
}

#[test]
fn quit_dialog_arrows_toggle_enter_confirms_esc_cancels() {
    // Kata app.md: Ctrl+q asks for confirmation; only YES + Enter quits.
    let mut app = test_app(&[1]);
    app.search_focus = false;
    app.on_key(key(KeyCode::Char('q'), KeyModifiers::ALT)).expect("alt+q");
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)).expect("enter");
    assert!(!app.quit, "Enter on the preselected NO does not quit");
    assert_eq!(app.confirm_quit, None, "NO closes the dialog");

    app.on_key(key(KeyCode::Char('q'), KeyModifiers::ALT)).expect("alt+q");
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE)).expect("esc");
    assert!(!app.quit, "Esc cancels");
    assert_eq!(app.confirm_quit, None, "Esc closes the dialog");

    app.on_key(key(KeyCode::Char('q'), KeyModifiers::ALT)).expect("alt+q");
    let before = screen_contents(app.tabs[0].active_shell());
    app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE)).expect("x");
    assert_eq!(
        screen_contents(app.tabs[0].active_shell()),
        before,
        "keys are swallowed while the dialog is open"
    );
    app.on_key(key(KeyCode::Left, KeyModifiers::NONE)).expect("left");
    assert_eq!(app.confirm_quit, Some(true), "arrow moves the highlight to YES");
    app.on_key(key(KeyCode::Right, KeyModifiers::NONE)).expect("right");
    assert_eq!(app.confirm_quit, Some(false), "arrow toggles back to NO");
    app.on_key(key(KeyCode::Left, KeyModifiers::NONE)).expect("left");
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)).expect("enter");
    assert!(app.quit, "YES + Enter quits");
}

#[test]
fn shortcuts_navigate_shells_within_tab() {
    let mut app = test_app(&[2]);
    app.on_key(key(KeyCode::Down, KeyModifiers::ALT)).expect("alt+down");
    assert_eq!(app.tabs[0].active, 1);
    app.on_key(key(KeyCode::Up, KeyModifiers::ALT)).expect("alt+up");
    assert_eq!(app.tabs[0].active, 0);
    app.on_key(key(KeyCode::Char('F'), KeyModifiers::ALT | KeyModifiers::SHIFT)).expect("alt+shift+f");
    assert!(app.tabs[0].favorite, "Alt+Shift+f toggles favorite");
}

#[test]
fn mouse_click_selects_tab_and_shell() {
    // Kata app.md/bugs.md: clicking a tab row selects that tab and shell.
    let mut app = test_app(&[1, 2]);
    let click = |row| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(click(9)).expect("click subshell row");
    assert_eq!((app.active, app.tabs[1].active), (1, 1));
    app.on_mouse(click(2)).expect("click tab 0");
    assert_eq!(app.active, 0);
    // Release ends any armed drag.
    let up = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: 3,
        row: 2,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(up).expect("release");
    assert_eq!(app.dragging_tab, None);
}

#[test]
fn mouse_drag_reorders_tabs() {
    // Kata app.md: order of tabs can be changed by dragging with the mouse.
    let mut app = test_app(&[1, 1]);
    let (c0, c1) = (app.tabs[0].shells[0].pid, app.tabs[1].shells[0].pid);
    let event = |kind, row| MouseEvent { kind, column: 3, row, modifiers: KeyModifiers::NONE };
    app.on_mouse(event(MouseEventKind::Down(MouseButton::Left), 2)).expect("grab tab 0");
    app.on_mouse(event(MouseEventKind::Drag(MouseButton::Left), 6)).expect("drag to tab 1");
    app.on_mouse(event(MouseEventKind::Up(MouseButton::Left), 6)).expect("drop");
    assert_eq!((app.tabs[0].shells[0].pid, app.tabs[1].shells[0].pid), (c1, c0), "tabs swapped");
    assert_eq!(app.active, 1, "selection follows the dragged tab");
}

#[test]
fn mouse_drag_reorders_against_the_filtered_list_it_is_shown() {
    // `shown` holds indices into `tabs`, and a reorder renumbers them. One drag
    // emits many events, all drained between two frames, so a `shown` left
    // stale until the next render made every event after the first resolve its
    // row against the old order. With a filter on it is sparse — and the second
    // drop then landed the tab on a row belonging to a *hidden* tab, somewhere
    // the user could not have aimed at.
    let mut app = test_app(&[1, 1, 1, 1]);
    let names = ["/t/keepa", "/t/hideb", "/t/keepc", "/t/keepd"];
    for (tab, cwd) in app.tabs.iter_mut().zip(names) {
        tab.shells[0].cwd = Some(cwd.into());
    }
    app.search = "keep".into();
    app.refresh_shown();
    assert_eq!(app.shown, vec![0, 2, 3], "the hidden tab leaves `shown` sparse");
    let order = |app: &App| {
        app.tabs
            .iter()
            .map(|t| t.shells[0].cwd.clone().unwrap_or_default().display().to_string())
            .collect::<Vec<_>>()
    };
    let event = |kind, row| MouseEvent { kind, column: 3, row, modifiers: KeyModifiers::NONE };
    // Each single-shell tab is four rows tall, so the shown tabs start at rows
    // 2, 6 and 10. Grab the first and drop it on the last shown row...
    app.on_mouse(event(MouseEventKind::Down(MouseButton::Left), 2)).expect("grab keepa");
    app.on_mouse(event(MouseEventKind::Drag(MouseButton::Left), 10)).expect("drag to keepd");
    assert_eq!(order(&app), ["/t/hideb", "/t/keepc", "/t/keepd", "/t/keepa"]);
    // ...then, in the same drag, back to the first shown row — which is now
    // `keepc`, not the hidden `hideb` that stale indices would have picked.
    app.on_mouse(event(MouseEventKind::Drag(MouseButton::Left), 2)).expect("drag back to the top row");
    assert_eq!(
        order(&app),
        ["/t/hideb", "/t/keepa", "/t/keepc", "/t/keepd"],
        "the tab lands on the row it was dropped on, never above a filtered-out tab"
    );
}

#[test]
fn mouse_drag_resizes_sidebar() {
    // Kata app.md: side panel width is resizable by mouse.
    let mut app = test_app(&[1]);
    app.term_width = 80;
    let border = app.sidebar_width - 1;
    let event = |kind, column| MouseEvent { kind, column, row: 5, modifiers: KeyModifiers::NONE };
    app.on_mouse(event(MouseEventKind::Down(MouseButton::Left), border)).expect("grab border");
    app.on_mouse(event(MouseEventKind::Drag(MouseButton::Left), 39)).expect("drag");
    assert_eq!(app.sidebar_width, 40);
    app.on_mouse(event(MouseEventKind::Drag(MouseButton::Left), 2)).expect("drag past min");
    assert_eq!(app.sidebar_width, MIN_SIDEBAR_WIDTH, "clamped to minimum");
    app.on_mouse(event(MouseEventKind::Up(MouseButton::Left), 8)).expect("release");
    assert!(!app.dragging_sidebar);

    // The grab zone lives entirely inside the sidebar: the pane's own first
    // column starts a text selection, it is not a resize handle.
    let mut app = test_app(&[1]);
    app.term_width = 80;
    let pane_start = app.sidebar_width;
    app.on_mouse(event(MouseEventKind::Down(MouseButton::Left), pane_start)).expect("pane edge");
    assert!(!app.dragging_sidebar, "the pane's first column is not the resize handle");
    assert_eq!(app.selection.expect("selection").anchor.1, 0, "it selects from pane column 0");
    // Two columns of handle, both on the sidebar side.
    for col in [app.sidebar_width - 1, app.sidebar_width - 2] {
        let mut app = test_app(&[1]);
        app.term_width = 80;
        app.on_mouse(event(MouseEventKind::Down(MouseButton::Left), col)).expect("grab");
        assert!(app.dragging_sidebar, "column {col} grabs the border");
    }
}

#[test]
fn mouse_wheel_scrolls_tab_list_over_sidebar() {
    let mut app = test_app(&[1, 1, 1]);
    app.sidebar_rows = 10; // overflowing viewport
    let wheel = |kind| MouseEvent { kind, column: 3, row: 5, modifiers: KeyModifiers::NONE };
    app.on_mouse(wheel(MouseEventKind::ScrollDown)).expect("wheel down");
    assert_eq!(app.list_offset, 1);
    app.on_mouse(wheel(MouseEventKind::ScrollUp)).expect("wheel up");
    assert_eq!(app.list_offset, 0);
}

// ── performance invariants (many tabs must never freeze the UI) ──────────────

#[test]
fn sample_shells_covers_all_shells_over_the_window() {
    // Staggered /proc sampling: each frame samples only a slice of shells, and
    // the rolling cursor covers every shell over the 2-Hz window — so the 3N
    // /proc reads never land on one frame (that burst froze the UI at scale).
    let mut app = test_app(&[1, 2, 1]); // 4 shells total
    // One frame samples ceil(4 / 16) = 1 non-active shell + the active one.
    let total: usize = app.tabs.iter().map(|t| t.shells.len()).sum();
    let frames = (SAMPLE_EVERY.as_millis() / POLL_INTERVAL.as_millis()) as usize;
    let per_frame = total.div_ceil(frames).max(1);
    assert_eq!(per_frame, 1, "one non-active shell sampled per frame at this count");
    // Walk the cursor through every shell: each flat index must resolve back.
    let seen: std::collections::HashSet<(usize, usize)> = (0..total)
        .map(|n| {
            let idx = app.flat_index(n);
            app.proc_cursor = (n + 1) % total;
            idx
        })
        .collect();
    assert_eq!(seen.len(), total, "every shell reachable via the cursor");
}

#[test]
fn visible_tabs_covers_only_the_viewport() {
    // The render builds items only for tabs intersecting the viewport — with
    // many tabs, building every item every frame is what froze the UI.
    let mut app = test_app(&[1, 2, 1]); // heights 4, 6, 4
    app.sidebar_rows = 10; // 8 viewport rows (title + search + 8)
    // Tab 1 only partly fits: the list widget skips a tab it cannot draw
    // whole, so it is not visible — and not clickable either.
    assert_eq!(app.visible_tabs(), 0..1, "tab 1 would end below the fold");
    assert_eq!(app.tab_at_row(2 + 5), None, "the blank rows under tab 0 hit nothing");
    app.list_offset = 1;
    assert_eq!(app.visible_tabs(), 1..2);
    app.sidebar_rows = 12;
    assert_eq!(app.visible_tabs(), 1..3);
    app.sidebar_rows = 0; // degenerate sidebar renders nothing
    assert_eq!(app.visible_tabs(), 1..1);
}

#[test]
fn agent_scan_runs_off_thread_and_rotates_over_the_shells() {
    // Agent detection walks all of /proc and may open a database — far too much
    // blocking IO for a frame. The per-shell 2-Hz sweep must never trigger it,
    // and `tick_agent` must only hand a pid to the worker, never resolve it
    // inline. A plain shell resolves to no agent.
    let mut app = test_app(&[1, 1]);
    for tab in &mut app.tabs {
        for shell in &mut tab.shells {
            shell.sample_proc(); // cwd/process/cmdline only — no /proc-wide scan
        }
    }
    assert!(app.tabs.iter().all(|t| t.shells.iter().all(|s| s.agent.is_none())));
    // The request is posted, not answered: `tick_agent` returns without waiting.
    app.tick_agent();
    assert!(app.agent_probe.pending, "the on-screen shell's pid went to the worker");
    // A second tick asks nothing more while one request is still out.
    app.agent_probe.asked = Instant::now() - SAMPLE_EVERY * 2;
    app.tick_agent();
    assert!(app.agent_probe.pending, "only one probe is ever in flight");
    // The answer lands on the shell it was asked for: no agent under a shell.
    assert!(
        wait_for(
            || {
                app.tick_agent();
                !app.agent_probe.pending
            },
            Duration::from_secs(10)
        ),
        "worker answered"
    );
    assert!(app.tabs[0].active_shell().agent.is_none(), "no AI agent under a bare shell");
}

// ── ai (kata ai.md) ──────────────────────────────────────────────────────────

#[test]
fn every_supported_client_is_detected_with_model_sources() {
    // Kata ai.md: claude, openclaude and opencode each show their model in the
    // status bar, so each needs a spec with at least one model source.
    for comm in ["claude", "openclaude", "opencode"] {
        let spec = AGENTS.iter().find(|s| s.comm == comm).unwrap_or_else(|| panic!("{comm} detected"));
        assert!(!spec.sources.is_empty(), "{comm} has a model source");
    }
    // The label falls back to the client's own name when no source resolves, so
    // the status bar never goes blank on a detected agent.
    assert!(AGENTS.iter().all(|s| !s.comm.is_empty()));
}

#[test]
fn status_bar_marks_an_unanswered_nudge_beside_the_model() {
    // The auto feature types into the agent on the user's behalf; the status
    // bar has to show that it did, until the agent answers with output.
    // The mark rides on `nudged`, which the agent's next output clears — that
    // re-arming is covered by `auto_nudges_an_idle_agent_once_per_silence`.
    fn footer(shell: &Shell) -> Footer<'_> {
        Footer {
            index: 0,
            count: 1,
            shell,
            branch: None,
            width: 120,
            tabs_overflow: false,
            copy_mode: true,
            auto: true,
        }
    }
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].agent = Some(AgentInfo {
        name: "claude",
        model: "claude-opus-5".into(),
        pid: 1,
        context: None,
        status: None,
    });
    let quiet = line_text(&status_bar(footer(&app.tabs[0].shells[0])));
    assert!(quiet.contains("✳ claude-opus-5"), "the model is shown: {quiet:?}");
    assert!(!quiet.contains('⟳'), "nothing typed yet, no mark: {quiet:?}");
    for nudge in [Nudge::Compacting(Instant::now()), Nudge::Continued(Instant::now())] {
        app.tabs[0].shells[0].nudge = Some(nudge);
        let nudged = line_text(&status_bar(footer(&app.tabs[0].shells[0])));
        assert!(nudged.contains("✳ claude-opus-5 ⟳"), "the nudge is visible ({nudge:?}): {nudged:?}");
    }
    // Context usage (kata ai.md) sits between the model and the mark.
    let context = Some(Context { used: 123_456, max: Some(1_000_000) });
    app.tabs[0].shells[0].agent =
        Some(AgentInfo { name: "claude", model: "claude-opus-5".into(), pid: 1, context, status: None });
    let full = line_text(&status_bar(footer(&app.tabs[0].shells[0])));
    assert!(full.contains("✳ claude-opus-5 123k/1M ⟳"), "used/max: {full:?}");
    // The phase is named while compacting.
    app.tabs[0].shells[0].nudge = Some(Nudge::Compacting(Instant::now()));
    let compacting = line_text(&status_bar(footer(&app.tabs[0].shells[0])));
    assert!(compacting.contains("123k/1M ⟳ /compact"), "the step under way: {compacting:?}");
    app.tabs[0].shells[0].nudge = None;
    // With no agent detected there is nothing to mark, nudged or not.
    app.tabs[0].shells[0].agent = None;
    assert!(!line_text(&status_bar(footer(&app.tabs[0].shells[0]))).contains('⟳'), "no agent, no mark");
}

#[test]
fn status_bar_counts_down_to_the_nudge_and_paints_a_full_context_red() {
    // Kata ai.md: with the auto feature on, the footer shows how long until
    // the agent is nudged — from the client's own idle status — and the usage
    // turns red once the context is nearly full.
    fn footer(shell: &Shell, auto: bool) -> Footer<'_> {
        Footer {
            index: 0,
            count: 1,
            shell,
            branch: None,
            width: 120,
            tabs_overflow: false,
            copy_mode: true,
            auto,
        }
    }
    let mut app = test_app(&[1]);
    let idle = |ago: Duration| Some(Status::Idle(SystemTime::now() - ago));
    let agent = |context, status| {
        Some(AgentInfo { name: "claude", model: "claude-opus-5".into(), pid: 1, context, status })
    };
    let shell = &mut app.tabs[0].shells[0];
    shell.last_input = Instant::now() - IDLE_NUDGE * 2;
    // Idle 7 minutes: 3 minutes to go — with auto on only.
    shell.agent = agent(None, idle(Duration::from_secs(7 * 60)));
    let text = line_text(&status_bar(footer(shell, true)));
    assert!(
        text.contains("✳ claude-opus-5 ⏳ 3:00 → /compact") || text.contains("⏳ 2:59 → /compact"),
        "{text:?}"
    );
    assert!(!line_text(&status_bar(footer(shell, false))).contains('⏳'), "auto off: no countdown");
    // Busy: nothing to count.
    shell.agent = agent(None, Some(Status::Busy));
    assert!(!line_text(&status_bar(footer(shell, true))).contains('⏳'), "busy: no countdown");
    // Idle 10 s: too early to show (the fallback would flicker it otherwise).
    shell.agent = agent(None, idle(Duration::from_secs(10)));
    assert!(!line_text(&status_bar(footer(shell, true))).contains('⏳'), "too early to show");
    // Typing into the shell defers: idle for hours by status, typed 5 s ago.
    shell.agent = agent(None, idle(Duration::from_secs(3600)));
    shell.last_content_change = Instant::now() - IDLE_NUDGE * 2;
    shell.last_input = Instant::now() - Duration::from_secs(5);
    assert_eq!(shell.idle_for().map(|d| d.as_secs()), Some(5), "the user's typing bounds the wait");
    assert!(shell.countdown().is_none(), "and hides the countdown");
    shell.last_input = Instant::now() - IDLE_NUDGE * 2;
    // A nearly full context: the usage is painted red, and the countdown is
    // the short one — compaction comes after a minute of idling.
    let full = Some(Context { used: 850_000, max: Some(1_000_000) });
    shell.agent = agent(full, idle(Duration::from_secs(40)));
    let line = status_bar(footer(shell, true));
    let text = line_text(&line);
    assert!(text.contains("850k/1M ⏳ 0:20 → /compact") || text.contains("⏳ 0:19 → /compact"), "{text:?}");
    let red = line.spans.iter().find(|s| s.content.contains("850k/1M")).expect("usage span");
    assert_eq!(red.style.fg, Some(CONTEXT_WARN_FG), "nearly full reads red");
    assert!(!red.content.contains('✳'), "only the usage is red: {:?}", red.content);
    // Under the warning line the usage is plain.
    shell.agent = agent(Some(Context { used: 500_000, max: Some(1_000_000) }), idle(Duration::from_secs(40)));
    let line = status_bar(footer(shell, true));
    assert!(line.spans.iter().all(|s| s.style.fg != Some(CONTEXT_WARN_FG)), "half full is not red");
    let text = line_text(&line);
    assert!(
        text.contains("⏳ 9:20 → /compact") || text.contains("⏳ 9:19"),
        "half full: the long silence applies: {text:?}"
    );
    assert_eq!(clock(Duration::from_secs(605)), "10:05");
    assert_eq!(clock(Duration::from_secs(59)), "0:59");
}

#[test]
fn the_clients_own_status_decides_idle_over_the_screen_hash() {
    // Kata ai.md: Claude Code registers `idle`/`busy` with a timestamp; that
    // is exact where the screen hash is a guess, so it wins whenever present.
    let idle = "{\"pid\":7,\"status\":\"idle\",\"updatedAt\":1,\"statusUpdatedAt\":1789282238190}";
    assert_eq!(
        session_status(idle),
        Some(Status::Idle(UNIX_EPOCH + Duration::from_millis(1_789_282_238_190)))
    );
    assert_eq!(session_status("{\"status\":\"busy\",\"statusUpdatedAt\":5}"), Some(Status::Busy));
    assert_eq!(session_status("{\"status\":\"shell\"}"), Some(Status::Busy), "running a command is busy");
    assert_eq!(session_status("{\"pid\":7}"), None, "no status field: the screen hash stands in");

    let dir = tempfile::tempdir().expect("tempdir");
    let mut shell = test_shell(dir.path());
    shell.resized = Instant::now() - RESIZE_GRACE * 2;
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    let shell_pid = shell.pid.expect("shell pid");
    type_and_enter(&mut shell, "cat > /dev/null");
    assert!(wait_for(|| !shell.shell_fg(), Duration::from_secs(10)), "a program owns the tty");
    let pid = foreground_pid(shell_pid).expect("foreground program");
    let agent = |status| {
        Some(AgentInfo { name: "claude", model: "claude-opus-5".into(), pid, context: None, status })
    };
    shell.last_input = Instant::now() - IDLE_NUDGE * 2;

    // Screen still for an hour, but the client says busy: no nudge.
    shell.agent = agent(Some(Status::Busy));
    shell.last_content_change = Instant::now() - IDLE_NUDGE * 6;
    assert_eq!(shell.idle_for(), None);
    shell.nudge_if_idle();
    thread::sleep(Duration::from_millis(200));
    assert!(shell.nudge.is_none() && !screen_contents(&shell).contains(COMPACT_COMMAND), "busy: no nudge");
    // Screen changed just now, but the client has been idle for eleven
    // minutes: nudge.
    shell.agent = agent(Some(Status::Idle(SystemTime::now() - IDLE_NUDGE - Duration::from_secs(60))));
    shell.last_content_change = Instant::now();
    shell.nudge_if_idle();
    assert!(matches!(shell.nudge, Some(Nudge::Compacting(_))), "idle by status: nudged");
    assert!(
        wait_for(|| screen_contents(&shell).contains(COMPACT_COMMAND), Duration::from_secs(10)),
        "the compact command was typed"
    );
    // The nudge itself restarts the wait, whatever the (stale) status says.
    assert!(shell.idle_for().expect("idle") < Duration::from_secs(5), "a nudge starts the clock over");
    // Compaction done: the client reports idle again *after* the command.
    let at = Instant::now() - Duration::from_secs(30);
    assert!(!shell.compaction_done(at), "idle since before the command is not done");
    shell.agent = agent(Some(Status::Idle(SystemTime::now() - Duration::from_secs(5))));
    assert!(shell.compaction_done(at), "idle again since the command: done");
}

#[test]
fn a_full_context_is_compacted_after_a_short_idle_once_per_window() {
    // Kata ai.md: past 70 % of the window, an agent idle for a minute is
    // compacted proactively — and not again until the usage can have dropped.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut shell = test_shell(dir.path());
    shell.resized = Instant::now() - RESIZE_GRACE * 2;
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    let shell_pid = shell.pid.expect("shell pid");
    type_and_enter(&mut shell, "cat > /dev/null");
    assert!(wait_for(|| !shell.shell_fg(), Duration::from_secs(10)), "a program owns the tty");
    let pid = foreground_pid(shell_pid).expect("foreground program");
    let agent = |used| {
        let context = Some(Context { used, max: Some(1_000_000) });
        Some(AgentInfo { name: "claude", model: "claude-opus-5".into(), pid, context, status: None })
    };
    shell.last_input = Instant::now() - IDLE_NUDGE * 2;
    // Half full, idle two minutes: not yet.
    shell.agent = agent(500_000);
    shell.last_content_change = Instant::now() - CONTEXT_SETTLE * 2;
    assert!(!shell.nudge_due());
    // 70 % full, idle two minutes: compact now.
    shell.agent = agent(700_000);
    assert!(shell.context_full() && shell.nudge_due());
    shell.nudge_if_idle();
    assert!(matches!(shell.nudge, Some(Nudge::Compacting(_))));
    assert!(
        wait_for(|| screen_contents(&shell).contains(COMPACT_COMMAND), Duration::from_secs(10)),
        "the compact command was typed"
    );
    assert!(shell.context_compacted.is_some(), "the context trigger is remembered");
    // Idle again with the (not yet refreshed) usage: the short trigger is
    // spent, only the full silence nudges.
    shell.nudge = None;
    shell.last_nudge = None;
    shell.last_content_change = Instant::now() - CONTEXT_SETTLE * 2;
    assert!(!shell.nudge_due(), "not twice within the window");
    shell.last_content_change = Instant::now() - IDLE_NUDGE;
    assert!(shell.nudge_due(), "the long silence still does");
    shell.context_compacted = Some(Instant::now() - CONTEXT_COMPACT_EVERY);
    shell.last_content_change = Instant::now() - CONTEXT_SETTLE;
    assert!(shell.nudge_due(), "and the window over, the short one is back");
    assert_eq!(Context { used: 700_000, max: Some(1_000_000) }.fill(), Some(0.7));
    assert_eq!(Context { used: 1, max: None }.fill(), None);
}

#[test]
fn an_idle_agent_off_screen_pings_once_per_stretch() {
    // Kata ai.md: an agent that goes idle in a tab that is not on screen
    // rings the host once; on screen it is simply seen; busy again resets.
    let mut app = test_app(&[1]);
    let shell = &mut app.tabs[0].shells[0];
    let idle = |ago: u64| Some(Status::Idle(SystemTime::now() - Duration::from_secs(ago)));
    let agent = |status| {
        Some(AgentInfo { name: "claude", model: "claude-opus-5".into(), pid: 1, context: None, status })
    };
    shell.last_input = Instant::now() - IDLE_NUDGE;
    shell.agent = agent(Some(Status::Busy));
    shell.tick_ping(false);
    assert!(!shell.pinged, "busy: nothing");
    shell.agent = agent(idle(1));
    shell.tick_ping(false);
    assert!(!shell.pinged, "a beat of debounce first");
    shell.agent = agent(idle(PING_AFTER_STATUS.as_secs()));
    shell.tick_ping(false);
    assert!(shell.pinged, "idle off screen: pinged");
    shell.tick_ping(false);
    assert!(shell.pinged, "and only once");
    shell.agent = agent(Some(Status::Busy));
    shell.tick_ping(false);
    assert!(!shell.pinged, "busy again: armed for the next stretch");
    shell.agent = agent(idle(60));
    shell.tick_ping(true);
    assert!(shell.pinged, "seen on screen: marked as told, no ring later");
    // Without a status the screen hash needs a full minute of stillness.
    shell.agent = agent(None);
    shell.pinged = false;
    shell.last_content_change = Instant::now() - Duration::from_secs(30);
    shell.tick_ping(false);
    assert!(!shell.pinged, "30 s of a still screen is a tool call, not idleness");
    shell.last_content_change = Instant::now() - PING_AFTER_SCREEN;
    shell.tick_ping(false);
    assert!(shell.pinged);
    // What goes to the host: a bell, then every notification dialect.
    let notice = idle_notice("claude", "ricon");
    assert!(notice.starts_with('\x07'), "bell first");
    for route in [
        "\x1b]9;ricon: claude in ricon is waiting for you\x07",
        "\x1b]777;notify;ricon;",
        "\x1b]99;i=1:p=body;claude in ricon",
    ] {
        assert!(notice.contains(route), "{route:?} in {notice:?}");
    }
}

#[test]
fn alt_question_mark_shows_the_cheat_sheet_until_any_key() {
    // Kata app.md: Alt+? (or Alt+h) lists every shortcut; the next key or
    // click closes it, and nothing leaks to the shell meanwhile.
    let mut app = test_app(&[1]);
    app.search_focus = false;
    let shell = app.tabs[0].active_shell();
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    thread::sleep(Duration::from_millis(200)); // let the prompt land whole
    app.on_key(key(KeyCode::Char('?'), KeyModifiers::ALT | KeyModifiers::SHIFT)).expect("alt+?");
    assert!(app.help, "alt+? opens the cheat sheet");
    let buf = render(&mut app, 100, 30);
    let screen: Vec<String> = (0..30).map(|y| row_text(&buf, y, 100)).collect();
    assert!(screen.iter().any(|r| r.contains(" shortcuts ")), "titled: {screen:#?}");
    for (keys, what) in SHORTCUTS {
        assert!(screen.iter().any(|r| r.contains(keys) && r.contains(what)), "{keys} — {what} listed");
    }
    let before = screen_contents(app.tabs[0].active_shell());
    app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE)).expect("x");
    assert!(!app.help, "any key closes it");
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        screen_contents(app.tabs[0].active_shell()),
        before,
        "the closing key never reached the shell"
    );
    app.on_key(key(KeyCode::Char('h'), KeyModifiers::ALT)).expect("alt+h");
    assert!(app.help, "alt+h opens it too");
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 50,
        row: 10,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(click).expect("click");
    assert!(!app.help, "a click closes it");
    assert!(app.selection.is_none() && app.dragging_tab.is_none(), "the click went nowhere else");
}

#[test]
fn claude_and_openclaude_read_the_live_session_model_first() {
    // Kata ai.md: the status bar shows the *model*. `settings.json` usually
    // carries none (the model is chosen in-session) and the env var freezes at
    // launch, so the session transcript — one JSONL per session under
    // `projects/<cwd slug>`, every answer stamped with the model that produced
    // it — is what keeps the bar truthful across a `/model` switch.
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = std::env::current_dir().expect("cwd");
    let slug: String = cwd
        .to_str()
        .expect("utf8 cwd")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    for (client, rel) in [("claude", ".claude"), ("openclaude", ".openclaude")] {
        let project = dir.path().join(rel).join("projects").join(&slug);
        std::fs::create_dir_all(&project).expect("project dir");
        // An older session, then the live one: the mid-session switch, the
        // `<synthetic>` placeholder (no model produced it) and a subagent's own
        // model must all be honoured.
        std::fs::write(project.join("old.jsonl"), "{\"model\":\"stale-model\"}\n").expect("old session");
        std::fs::write(
            project.join("live.jsonl"),
            format!(
                "{{\"model\":\"{client}-first\"}}\n\
                 {{\"model\":\"{client}-now\"}}\n\
                 {{\"model\":\"<synthetic>\"}}\n\
                 {{\"isSidechain\":true,\"model\":\"subagent-model\"}}\n"
            ),
        )
        .expect("live session");
        // Make the live session the newest even on a coarse-grained clock.
        let _ = std::fs::File::options()
            .write(true)
            .open(project.join("live.jsonl"))
            .and_then(|f| f.set_modified(std::time::SystemTime::now() + Duration::from_secs(1)));
    }
    with_home(dir.path(), || {
        for client in ["claude", "openclaude"] {
            let spec = AGENTS.iter().find(|s| s.comm == client).expect("spec");
            let model = spec.sources.iter().find_map(|src| resolve_source(src, std::process::id()));
            assert_eq!(model.as_deref(), Some(format!("{client}-now").as_str()), "{client}");
        }
    });
}

#[test]
fn claude_and_openclaude_read_their_settings_model() {
    // Both keep the selected model in a `model` key of their own settings file
    // under `$HOME` — that is what the status bar shows.
    let dir = tempfile::tempdir().expect("tempdir");
    for (client, rel) in [("claude", ".claude"), ("openclaude", ".openclaude")] {
        std::fs::create_dir_all(dir.path().join(rel)).expect("config dir");
        std::fs::write(
            dir.path().join(rel).join("settings.json"),
            format!("{{\n \"env\": {{}},\n \"model\": \"{client}-model\"\n}}"),
        )
        .expect("settings");
    }
    with_home(dir.path(), || {
        for client in ["claude", "openclaude"] {
            let spec = AGENTS.iter().find(|s| s.comm == client).expect("spec");
            // `pid` is unused by a settings source; resolution stops at the
            // first source that answers, exactly as the probe thread does.
            let model = spec.sources.iter().find_map(|src| resolve_source(src, std::process::id()));
            assert_eq!(model.as_deref(), Some(format!("{client}-model").as_str()));
        }
    });
}

#[test]
fn claude_resolves_its_own_session_by_pid_before_the_newest_file() {
    // Kata ai.md: the model shown is *this* tab's. Two sessions open in the
    // same project each write their own transcript, and the newest file is
    // whichever answered last — so the client's own registration under
    // `sessions/<pid>.json` decides, and the newest file is only the fallback.
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = std::env::current_dir().expect("cwd");
    let slug: String = cwd
        .to_str()
        .expect("utf8 cwd")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let pid = std::process::id();
    let project = dir.path().join(".claude/projects").join(&slug);
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::create_dir_all(dir.path().join(".claude/sessions")).expect("sessions dir");
    let mine = "{\"type\":\"assistant\",\"message\":{\"model\":\"mine-model\",\"usage\":{\"input_tokens\":1000,\
                \"cache_creation_input_tokens\":20000,\"cache_read_input_tokens\":100000,\"output_tokens\":7,\
                \"iterations\":[{\"input_tokens\":999999}]}}}\n";
    std::fs::write(project.join("mine.jsonl"), mine).expect("own session");
    std::fs::write(project.join("newer.jsonl"), "{\"model\":\"other-model\"}\n").expect("other session");
    let _ = std::fs::File::options()
        .write(true)
        .open(project.join("newer.jsonl"))
        .and_then(|f| f.set_modified(std::time::SystemTime::now() + Duration::from_secs(1)));
    std::fs::write(
        dir.path().join(format!(".claude/sessions/{pid}.json")),
        format!("{{\"pid\":{pid},\"sessionId\":\"mine\",\"cwd\":\"{}\"}}", cwd.display()),
    )
    .expect("registration");
    with_home(dir.path(), || {
        assert_eq!(session_tail_model(".claude/projects", pid).as_deref(), Some("mine-model"));
        // Context usage rides on the same file: the last answer's input, the
        // cache it wrote and the cache it read — not the per-iteration copies.
        let context = session_context(".claude/projects", pid).expect("usage");
        assert_eq!(context, Context { used: 121_000, max: Some(CONTEXT_WINDOW) });
        // A `[1m]` model in the client's settings means the wide window.
        std::fs::write(dir.path().join(".claude/settings.json"), "{\"model\":\"claude-opus-5[1m]\"}")
            .expect("settings");
        assert_eq!(session_context(".claude/projects", pid).expect("usage").max, Some(WIDE_CONTEXT_WINDOW));
        // A registered session with no transcript yet (new, or just after
        // `/clear`) has no model to show — the newest file is someone else's.
        std::fs::write(dir.path().join(format!(".claude/sessions/{pid}.json")), "{\"sessionId\":\"gone\"}")
            .expect("unwritten registration");
        assert_eq!(session_tail_model(".claude/projects", pid), None);
        assert_eq!(session_context(".claude/projects", pid), None, "nor its usage");
        // A client that registers nothing falls back to the newest file.
        std::fs::remove_file(dir.path().join(format!(".claude/sessions/{pid}.json"))).expect("unregister");
        assert_eq!(session_tail_model(".claude/projects", pid).as_deref(), Some("other-model"));
    });
}

#[test]
fn session_usage_and_token_labels() {
    // The usage of the last main-session answer; a usage past the narrow
    // window proves the wide one.
    let text = "{\"usage\":{\"input_tokens\":5,\"cache_creation_input_tokens\":6,\"cache_read_input_tokens\":7}}\n\
                {\"isSidechain\":true,\"usage\":{\"input_tokens\":1,\"cache_creation_input_tokens\":1,\"cache_read_input_tokens\":1}}\n\
                {\"type\":\"user\"}\n";
    assert_eq!(last_session_usage(text), Some(18));
    assert_eq!(
        last_session_usage("{\"usage\":{\"output_tokens\":3}}"),
        Some(0),
        "missing fields count as zero"
    );
    assert_eq!(last_session_usage("no usage here"), None);
    assert_eq!(json_number("{\"a\": 12,\"b\":3}", "b"), Some(3));
    assert_eq!(json_number("{\"a\": 12}", "a"), Some(12));
    assert_eq!(json_number("{\"ab\":12}", "a"), None);
    for (n, label) in [
        (0, "0"),
        (999, "999"),
        (1_000, "1k"),
        (123_456, "123k"),
        (199_500, "200k"),
        (999_499, "999k"),
        (999_500, "1M"),
        (1_000_000, "1M"),
        (1_250_000, "1.3M"),
        (2_040_000, "2M"),
    ] {
        assert_eq!(tokens(n), label, "{n}");
    }
    assert_eq!(context_label(Some(Context { used: 42_000, max: Some(200_000) })), " 42k/200k");
    assert_eq!(context_label(Some(Context { used: 42_000, max: None })), " 42k");
    assert_eq!(context_label(None), "");
}

#[test]
fn probe_rotation_reaches_every_shell_not_just_the_visible_one() {
    // The auto feature nudges idle agents in any tab, so every shell must get
    // its agent resolved — while the on-screen shell (whose model the status
    // bar draws) still comes up every other turn.
    let mut app = test_app(&[1, 1, 1]);
    let pids: Vec<u32> = app.tabs.iter().filter_map(|t| t.shells[0].pid).collect();
    let on_screen = pids[0];
    let probed: Vec<u32> = (0..12).filter_map(|_| app.next_probe_target()).collect();
    for pid in &pids {
        assert!(probed.contains(pid), "every shell is probed in turn: {probed:?}");
    }
    assert!(
        probed.iter().filter(|&&p| p == on_screen).count() >= probed.len() / 2,
        "the visible shell keeps every other turn: {probed:?}"
    );
}

#[test]
fn auto_button_is_glued_to_the_right_edge_of_the_panel() {
    // Kata ai.md: the button sits after the first line's text and indicators,
    // glued to the panel's right edge — one column for every tab, whatever its
    // name, star or unseen marker. Render and hit-test share that one span.
    const W: u16 = 100;
    const H: u16 = 24;
    assert_eq!(AUTO_BUTTON.chars().count() as u16, AUTO_COLS);
    let span = auto_span(SIDEBAR_WIDTH);
    assert_eq!(span.len(), AUTO_COLS as usize);
    assert_eq!(span.end, SIDEBAR_WIDTH as usize - 1, "flush against the panel border, never over it");
    let mut app = test_app(&[2]);
    let drawn = |app: &mut App| {
        let buf = render(app, W, H);
        (span.clone()).map(|x| cell(&buf, x as u16, 2).symbol().to_string()).collect::<String>()
    };
    // Name length, a favorite's ⭐ and the unseen `*` all leave it put.
    for (cwd, favorite, unseen) in
        [("/tmp/alpha", false, false), ("/tmp/a-very-long-folder-name-indeed", true, true)]
    {
        app.tabs[0].shells[0].cwd = Some(cwd.into());
        app.tabs[0].favorite = favorite;
        app.tabs[0].shells[1].unseen_output = unseen;
        assert_eq!(drawn(&mut app), AUTO_BUTTON, "the button owns exactly its span ({cwd})");
    }
}

#[test]
fn auto_button_column_matches_its_hit_test_at_every_panel_width() {
    // The button's whole contract is that what is drawn is what is clickable.
    // A narrow panel used to break it two ways: the span claimed the border
    // column (so the sidebar could not be dragged on a tab-name row), and a
    // name truncated to a bare `…` overflowed its budget and shoved the button
    // one column right of where the hit-test looked.
    const W: u16 = 120;
    const H: u16 = 24;
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].cwd = Some("/tmp/alpha".into());
    for width in MIN_SIDEBAR_WIDTH..=40 {
        app.sidebar_width = width;
        let span = auto_span(width);
        assert!(span.end < width as usize, "width {width}: the span must stop short of the border column");
        let Some(button) = auto_button(0, &app.tabs[0], width) else { continue };
        let buf = render(&mut app, W, H);
        let drawn: String = button.clone().map(|x| cell(&buf, x as u16, 2).symbol()).collect();
        assert_eq!(
            drawn, AUTO_BUTTON,
            "width {width}: the button occupies exactly the span its hit-test reads"
        );
    }
}

#[test]
fn auto_button_is_dropped_when_the_panel_cannot_seat_it() {
    // At the narrowest panel the row's own margins reach the button's fixed
    // column first. Drawing it short of that column would answer clicks
    // nowhere near where it sits, so it is dropped — and then nothing in the
    // sidebar hit-tests as the button either.
    let app = test_app(&[1]);
    assert_eq!(auto_button(0, &app.tabs[0], MIN_SIDEBAR_WIDTH), None);
    assert_eq!(app.auto_at(2, 0), None, "no button drawn, so no column toggles one");
    assert!(auto_button(0, &app.tabs[0], SIDEBAR_WIDTH).is_some(), "the default panel seats it");
}

#[test]
fn auto_button_shows_its_state_in_its_background() {
    // Kata ai.md: on each tab's first line — enabled = green background,
    // disabled = gray.
    const W: u16 = 100;
    const H: u16 = 24;
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].cwd = Some("/tmp/alpha".into());
    for auto in [true, false] {
        app.tabs[0].auto = auto;
        let buf = render(&mut app, W, H);
        // Tab rows start under the " ricon " title and the search row.
        let row = row_text(&buf, 2, app.sidebar_width);
        assert!(row.contains(AUTO_BUTTON.trim_end()), "drawn on the tab's first line: {row:?}");
        let want = if auto { AUTO_ON_BG } else { AUTO_OFF_BG };
        let span = auto_span(app.sidebar_width);
        for x in span.clone() {
            let drawn = cell(&buf, x as u16, 2);
            assert_eq!(drawn.bg, want, "column {x} of the auto button (auto={auto})");
            // The label has to stay legible on whichever background it carries.
            assert_ne!(drawn.fg, drawn.bg, "column {x} of the auto button (auto={auto})");
        }
        assert!(
            matches!(AUTO_ON_BG, Color::Rgb(r, g, b) if g > r + 40 && g > b + 40),
            "enabled reads as green: {AUTO_ON_BG:?}"
        );
        assert_eq!(AUTO_OFF_BG, Color::DarkGray, "disabled reads as gray");
        assert_ne!(cell(&buf, span.end as u16, 2).bg, want, "the tab row beside it keeps its own style");
    }
}

#[test]
fn auto_button_marks_a_nudge_the_agent_has_not_answered() {
    // The auto feature types into agents in tabs that are off screen, so the
    // sidebar has to show where it spoke — in the label's color only, since the
    // button's width is what `auto_span` hit-tests.
    const W: u16 = 100;
    const H: u16 = 24;
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].cwd = Some("/tmp/alpha".into());
    app.tabs[0].auto = true; // only a tab with the feature on can be nudged
    let span = auto_span(app.sidebar_width);
    let label_fg = |app: &mut App| cell(&render(app, W, H), span.start as u16, 2).fg;
    assert_eq!(label_fg(&mut app), AUTO_ON_FG, "quiet agent: the plain label");
    app.tabs[0].shells[0].nudge = Some(Nudge::Compacting(Instant::now()));
    assert_eq!(label_fg(&mut app), AUTO_NUDGED_FG, "an unanswered nudge is visible from the sidebar");
    // Layout is untouched by the mark — only the color moved.
    assert_eq!(auto_span(app.sidebar_width), span, "the hit-test does not move");
    // With the feature off the button reads as off, whatever it typed earlier.
    app.tabs[0].auto = false;
    assert_eq!(label_fg(&mut app), AUTO_OFF_FG, "disabled reads as disabled");
}

#[test]
fn auto_toggles_per_tab_from_its_button_and_persists() {
    // Kata ai.md: off by default on a new tab, one button per tab, and the
    // choice survives a restart via the session file.
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let mut app = test_app(&[1, 1]);
        assert!(app.tabs.iter().all(|t| !t.auto), "off by default on a new tab");
        let span = auto_span(app.sidebar_width);
        let click = |column: usize| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: column as u16,
            row: 2, // tab 0's first line, under the title and search rows
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(click(span.start)).expect("click auto");
        assert!(app.tabs[0].auto, "the button toggles that tab's feature on");
        assert!(!app.tabs[1].auto, "the other tab keeps its own state");
        assert!(load_session().tabs[0].auto, "and the choice is persisted at once");
        app.on_mouse(click(span.end - 1)).expect("click auto, last column");
        assert!(!app.tabs[0].auto && !load_session().tabs[0].auto, "toggling back is persisted too");
        // The click lands on the button, not the tab row under it.
        assert!(app.dragging_tab.is_none(), "the auto button never arms a tab drag");
    });
}

#[test]
fn auto_nudges_an_idle_agent_once_per_silence() {
    // Kata ai.md: a supported client with no output change for ten minutes is
    // typed the compact command — once, until it produces output again — and
    // the continue text only once that compaction has settled.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut shell = test_shell(dir.path());
    shell.resized = Instant::now() - RESIZE_GRACE * 2;
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    shell.tick_activity(true);

    shell.last_content_change = Instant::now() - IDLE_NUDGE;
    shell.nudge_if_idle();
    assert!(!screen_contents(&shell).contains(COMPACT_COMMAND), "no agent in the shell: no nudge");

    // A command typed at a bare prompt would be run by the shell, so the tty
    // must belong to the agent: stand one in with a program that owns it.
    let agent = |pid| {
        Some(AgentInfo { name: "claude", model: "claude-opus-5".into(), pid, context: None, status: None })
    };
    let shell_pid = shell.pid.expect("shell pid");
    shell.agent = agent(shell_pid);
    shell.nudge_if_idle();
    assert!(!screen_contents(&shell).contains(COMPACT_COMMAND), "shell at its prompt: no nudge");
    type_and_enter(&mut shell, "cat > /dev/null");
    assert!(wait_for(|| !shell.shell_fg(), Duration::from_secs(10)), "a program owns the tty");
    // Some other program holding the tty is not the agent: an agent that has
    // been suspended or backgrounded must never have anything typed at it.
    shell.last_content_change = Instant::now() - IDLE_NUDGE;
    shell.nudge = None;
    shell.nudge_if_idle();
    thread::sleep(Duration::from_millis(200));
    assert!(!screen_contents(&shell).contains(COMPACT_COMMAND), "the agent does not own the tty: no nudge");
    // Now the agent is the process reading the tty (the tty check is
    // throttled, and the refusal above just spent it).
    shell.agent = agent(foreground_pid(shell_pid).expect("foreground program"));
    shell.last_content_change = Instant::now() - IDLE_NUDGE;
    shell.nudge = None;
    shell.tty_checked.set(stale(SAMPLE_EVERY));
    shell.nudge_if_idle();
    assert!(matches!(shell.nudge, Some(Nudge::Compacting(_))), "the compact step is recorded");
    assert!(
        wait_for(|| screen_contents(&shell).contains(COMPACT_COMMAND), Duration::from_secs(10)),
        "the compact command was typed: {:?}",
        screen_contents(&shell)
    );
    let typed = |shell: &Shell| screen_contents(shell).matches(COMPACT_COMMAND).count();
    let once = typed(&shell);
    // The nudge itself starts a new silence, so the next frame types nothing:
    // one nudge per silence, never one per frame.
    shell.nudge_if_idle();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(typed(&shell), once, "one nudge per silence");
    // The client's echo of the command is fresh content — the compaction under
    // way. It must not take the mark down, and nothing more is typed until the
    // screen has settled after it.
    shell.tick_activity(true);
    assert!(matches!(shell.nudge, Some(Nudge::Compacting(_))), "still compacting");
    assert!(shell.last_content_change.elapsed() < IDLE_NUDGE, "new content re-arms the idle clock");
    shell.nudge_if_idle();
    thread::sleep(Duration::from_millis(200));
    assert!(!screen_contents(&shell).contains(DEFAULT_CONTINUE), "a compaction under way is not interrupted");
    // Settled: the screen changed after the command and then stood still.
    shell.nudge = Some(Nudge::Compacting(Instant::now() - COMPACT_SETTLE * 2));
    shell.last_content_change = Instant::now() - COMPACT_SETTLE;
    shell.nudge_if_idle();
    assert!(matches!(shell.nudge, Some(Nudge::Continued(_))), "the continue step is recorded");
    assert!(
        wait_for(|| screen_contents(&shell).contains(DEFAULT_CONTINUE), Duration::from_secs(10)),
        "the continue text was typed: {:?}",
        screen_contents(&shell)
    );
    // The agent's echo of the text is fresh content: it re-arms the clock, but
    // it must not take the mark down with it — it landed milliseconds after
    // the nudge, and a mark nobody can see is a nudge typed invisibly.
    shell.tick_activity(true);
    assert!(shell.nudge.is_some(), "the mark outlives the agent's own echo");
    shell.nudge_if_idle();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(typed(&shell), once, "a busy agent is never interrupted");
    // Once the mark has had its time on screen, the agent's next answer clears it.
    shell.nudge = Some(Nudge::Continued(Instant::now() - NUDGE_MARK));
    let seen = shell.activity.load(Ordering::Relaxed);
    shell.send(b"answered\r");
    assert!(
        wait_for(|| shell.activity.load(Ordering::Relaxed) != seen, Duration::from_secs(10)),
        "the agent answers"
    );
    shell.tick_activity(true);
    shell.nudge_if_idle();
    assert!(shell.nudge.is_none(), "an answered nudge, once seen, clears the mark");
    // An agent that answered within the mark's time and has been silent ever
    // since is cleared too — waiting for more output would hold the mark (and
    // block every later nudge) for good.
    let at = Instant::now() - NUDGE_MARK;
    shell.nudge = Some(Nudge::Continued(at));
    shell.last_content_change = at + Duration::from_secs(1);
    shell.nudge_if_idle();
    assert!(shell.nudge.is_none(), "a quick answer clears the mark once it has been seen");
    // Switching auto off abandons a nudge under way, mark included.
    let mut tab = Tab { shells: vec![shell], active: 0, favorite: false, auto: false };
    tab.shells[0].nudge = Some(Nudge::Compacting(Instant::now()));
    tab.nudge_idle_agents();
    assert!(tab.shells[0].nudge.is_none(), "auto off: no half-done nudge survives");
}

#[test]
fn a_compaction_that_never_answers_times_out_into_continue() {
    // A client without the command (or one that swallowed it) shows no change
    // at all; the continue text still follows, after `COMPACT_TIMEOUT`.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut shell = test_shell(dir.path());
    shell.resized = Instant::now() - RESIZE_GRACE * 2;
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    let shell_pid = shell.pid.expect("shell pid");
    type_and_enter(&mut shell, "cat > /dev/null");
    assert!(wait_for(|| !shell.shell_fg(), Duration::from_secs(10)), "a program owns the tty");
    let pid = foreground_pid(shell_pid).expect("foreground program");
    shell.agent =
        Some(AgentInfo { name: "claude", model: "claude-opus-5".into(), pid, context: None, status: None });
    let at = Instant::now() - COMPACT_TIMEOUT;
    shell.nudge = Some(Nudge::Compacting(at));
    shell.last_content_change = at; // nothing changed since the command
    assert!(shell.compaction_done(at), "timed out");
    shell.nudge_if_idle();
    assert!(matches!(shell.nudge, Some(Nudge::Continued(_))));
    assert!(
        wait_for(|| screen_contents(&shell).contains(DEFAULT_CONTINUE), Duration::from_secs(10)),
        "continue typed after the timeout: {:?}",
        screen_contents(&shell)
    );
    // The agent gone mid-nudge: the second step is dropped, not typed at
    // whatever took the tty.
    shell.nudge = Some(Nudge::Compacting(at));
    shell.agent = Some(AgentInfo {
        name: "claude",
        model: "claude-opus-5".into(),
        pid: 1,
        context: None,
        status: None,
    });
    shell.nudge_if_idle();
    assert!(shell.nudge.is_none(), "no agent on the tty: the nudge is abandoned");
}

#[test]
fn the_continue_text_comes_from_the_projects_auto_file_when_it_has_one() {
    // Kata ai.md: after the compaction, `.ai/auto.md` in the project says what
    // to type; without it (or empty) the plain word `continue` is typed.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = tempfile::tempdir().expect("config dir");
    let custom = "keep going with the plan\nthen run the tests";
    with_config_home(config.path(), || {
        assert_eq!(continue_text(None), DEFAULT_CONTINUE);
        assert_eq!(continue_text(Some(dir.path())), DEFAULT_CONTINUE, "no file: the default");
        std::fs::create_dir_all(dir.path().join(".ai")).expect(".ai dir");
        std::fs::write(dir.path().join(AUTO_FILE), " \n\n").expect("blank file");
        assert_eq!(continue_text(Some(dir.path())), DEFAULT_CONTINUE, "a blank file: the default");
        // The user-wide file steps in for any project without one of its own.
        std::fs::create_dir_all(config.path().join("ricon")).expect("ricon config dir");
        std::fs::write(config.path().join("ricon/auto.md"), "user-wide text\n").expect("global auto.md");
        assert_eq!(continue_text(Some(dir.path())), "user-wide text", "a blank project file: the global one");
        assert_eq!(continue_text(None), "user-wide text", "no project at all: the global one");
        std::fs::write(dir.path().join(AUTO_FILE), format!("\n{custom}\n\n")).expect("auto.md");
        assert_eq!(continue_text(Some(dir.path())), custom, "the project's file wins, trimmed");
    });

    // Typed into the agent for real: the agent stand-in runs in the project.
    let mut shell = test_shell(dir.path());
    shell.resize(24, 120);
    shell.resized = Instant::now() - RESIZE_GRACE * 2;
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    let shell_pid = shell.pid.expect("shell pid");
    type_and_enter(&mut shell, "cat > /dev/null");
    assert!(wait_for(|| !shell.shell_fg(), Duration::from_secs(10)), "a program owns the tty");
    let pid = foreground_pid(shell_pid).expect("foreground program");
    shell.agent =
        Some(AgentInfo { name: "claude", model: "claude-opus-5".into(), pid, context: None, status: None });
    assert_eq!(shell.project_dir().as_deref(), Some(dir.path()), "the agent's own directory is the project");
    shell.nudge = Some(Nudge::Compacting(Instant::now() - COMPACT_SETTLE * 2));
    shell.last_content_change = Instant::now() - COMPACT_SETTLE;
    shell.nudge_if_idle();
    assert!(
        wait_for(|| screen_contents(&shell).contains("keep going with the plan"), Duration::from_secs(10)),
        "the file's first line was typed: {:?}",
        screen_contents(&shell)
    );
    assert!(
        wait_for(|| screen_contents(&shell).contains("then run the tests"), Duration::from_secs(10)),
        "and its second: {:?}",
        screen_contents(&shell)
    );
}

/// Put a probe in the shell that owns the tty and reports what it reads: each
/// `read()` is bracketed by `<`…`>` on screen, with CR shown as `R` and ESC as
/// `E` so the markers survive being printed to a raw tty. `bracketed` makes it
/// ask for bracketed paste, the way a full-screen AI client does. Returns the
/// probe's process group, which is what stands in for the agent.
fn read_probe(shell: &mut Shell, bracketed: bool) -> u32 {
    let mode = if bracketed { r#"printf "\033[?2004h"; "# } else { "" };
    // The probe wipes the screen first, so what is left on it is only ever what
    // the probe itself read back.
    type_and_enter(
        shell,
        &format!(
            r#"sh -c 'stty raw -echo; printf "\033[2J\033[H"; {mode}while :; do printf "<"; dd bs=4096 count=1 2>/dev/null | tr "\r\033" "RE"; printf ">"; done'"#
        ),
    );
    assert!(wait_for(|| probe_reads(shell).trim() == "<", Duration::from_secs(10)), "the probe is reading");
    if bracketed {
        assert!(
            wait_for(|| shell.modes().bracketed_paste, Duration::from_secs(10)),
            "the probe asked for bracketed paste"
        );
    }
    foreground_pid(shell.pid.expect("shell pid")).expect("probe process group")
}

/// What the probe has printed, with the line wrapping taken back out.
fn probe_reads(shell: &Shell) -> String {
    screen_contents(shell).replace('\n', "")
}

#[test]
fn a_nudge_is_confirmed_by_an_enter_of_its_own() {
    // Kata ai.md: the compact command and the continue text are not only
    // typed but *confirmed*. Full-screen clients (Claude Code and friends) read
    // their tty in chunks and take one chunk of many bytes as pasted text — a
    // `\r` riding along in that same chunk is a literal newline in the
    // composer, and the message just sits there unsent. So the confirmation
    // has to arrive as its own read.
    for bracketed in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut shell = test_shell(dir.path());
        shell.resize(24, 120);
        shell.resized = Instant::now() - RESIZE_GRACE * 2;
        assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
        let pid = read_probe(&mut shell, bracketed);
        shell.agent = Some(AgentInfo {
            name: "claude",
            model: "claude-opus-5".into(),
            pid,
            context: None,
            status: None,
        });

        for text in [COMPACT_COMMAND, DEFAULT_CONTINUE] {
            if text == COMPACT_COMMAND {
                shell.last_content_change = Instant::now() - IDLE_NUDGE;
            } else {
                // The continue step, as if the compaction had settled.
                shell.nudge = Some(Nudge::Compacting(Instant::now() - COMPACT_SETTLE * 2));
                shell.last_content_change = Instant::now() - COMPACT_SETTLE;
            }
            shell.nudge_if_idle();
            // The text lands in one read — bracketed when the client asked for
            // it, so it can only ever be content, never keys the composer acts on.
            let typed = if bracketed { format!("<E[200~{text}E[201~>") } else { format!("<{text}>") };
            assert!(
                wait_for(|| probe_reads(&shell).contains(&typed), Duration::from_secs(10)),
                "{text:?} arrives whole (bracketed: {bracketed}): {:?}",
                probe_reads(&shell)
            );
            // …and the Enter that sends it is a read of its own, not a byte
            // trailing the paste. This is the assertion the bug was hiding behind.
            assert!(
                wait_for(|| probe_reads(&shell).contains(&format!("{typed}<R>")), Duration::from_secs(10)),
                "{text:?} is confirmed by its own keypress (bracketed: {bracketed}): {:?}",
                probe_reads(&shell)
            );
        }
    }
}

#[test]
fn idle_is_the_screen_standing_still_not_the_absence_of_bytes() {
    // Kata ai.md: the silence the auto feature waits out is "console content is
    // the same". An agent waiting for the user keeps writing bytes that repaint
    // the very same screen (cursor blink, redraw ticks) — counting those as
    // activity would hold the ten-minute clock at zero and the nudge would
    // never fire for the clients the kata names.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut shell = test_shell(dir.path());
    shell.resized = Instant::now() - RESIZE_GRACE * 2;
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    shell.agent = Some(AgentInfo {
        name: "claude",
        model: "claude-opus-5".into(),
        pid: shell.pid.expect("shell pid"),
        context: None,
        status: None,
    });
    shell.tick_activity(true);
    let idle_since = shell.last_content_change;

    // Bytes that leave the screen exactly as it was: the shell is still idle.
    shell.activity.fetch_add(1, Ordering::Relaxed);
    shell.tick_activity(true);
    assert_eq!(shell.last_content_change, idle_since, "a repaint of the same screen is not activity");
    assert!(shell.last_change > idle_since, "the byte clock still moves — the spinner rides on it");

    // Content that actually changed does move it.
    type_and_enter(&mut shell, "echo alive");
    assert!(wait_for(|| screen_contents(&shell).contains("alive\n"), Duration::from_secs(10)), "output");
    shell.tick_activity(true);
    assert!(shell.last_content_change > idle_since, "changed content is activity");
}

#[test]
fn auto_off_types_nothing_at_an_idle_agent() {
    // Kata ai.md: the continue prompt is inserted only for a tab whose auto
    // feature is on — with the button off, an idle agent that meets every
    // other condition is left alone. The silence itself is the kata's ten
    // minutes, pinned here so the constant cannot drift from the rule again.
    assert_eq!(IDLE_NUDGE, Duration::from_secs(10 * 60), "kata ai.md: ten minutes of silence");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut tab = Tab::spawn(24, 120, dir.path(), None).expect("spawn tab");
    let shell = &mut tab.shells[0];
    shell.resized = Instant::now() - RESIZE_GRACE * 2;
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    // Hand the tty to a program that is not a shell and call it the agent, so
    // only the tab's flag stands between the silence and the prompt.
    let shell_pid = shell.pid.expect("shell pid");
    type_and_enter(shell, "cat > /dev/null");
    assert!(wait_for(|| !shell.shell_fg(), Duration::from_secs(10)), "a program owns the tty");
    let agent_pid = foreground_pid(shell_pid).expect("foreground program");
    shell.agent = Some(AgentInfo {
        name: "claude",
        model: "claude-opus-5".into(),
        pid: agent_pid,
        context: None,
        status: None,
    });

    tab.auto = false;
    tab.shells[0].last_content_change = Instant::now() - IDLE_NUDGE;
    tab.nudge_idle_agents();
    thread::sleep(Duration::from_millis(200));
    assert!(!screen_contents(&tab.shells[0]).contains(COMPACT_COMMAND), "auto off: nothing is typed");
    assert!(tab.shells[0].nudge.is_none(), "and no nudge is recorded");

    // The same tab with the button on does nudge: the flag is the only gate.
    tab.auto = true;
    tab.shells[0].last_content_change = Instant::now() - IDLE_NUDGE;
    tab.nudge_idle_agents();
    assert!(
        wait_for(|| screen_contents(&tab.shells[0]).contains(COMPACT_COMMAND), Duration::from_secs(10)),
        "auto on: the compact command was typed: {:?}",
        screen_contents(&tab.shells[0])
    );
}

#[test]
fn persist_now_is_io_free_and_uses_caches() {
    // Persisting snapshots the 2 Hz caches — it must not re-read /proc per
    // shell (that added O(shells) IO every second).
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let mut app = test_app(&[1]);
        app.tabs[0].shells[0].cwd = Some("/tmp".into());
        app.tabs[0].shells[0].fg_cmd = Some("make -j".into());
        app.persist_now();
        let saved = &app.saved_session.tabs[0].shells[0];
        assert_eq!(saved.cwd, PathBuf::from("/tmp"), "persists the cached cwd");
        assert_eq!(saved.cmd.as_deref(), Some("make -j"), "persists the cached command");
    });
}

// ── search row (kata ui.md) ──────────────────────────────────────────────────

#[test]
fn search_starts_focused_and_filters_tabs() {
    // Kata: the search row has focus when the app starts and filters tabs.
    let mut app = test_app(&[1, 1, 1]);
    assert!(app.search_focus, "focused at start");
    app.tabs[0].shells[0].cwd = Some("/tmp/alpha".into());
    app.tabs[1].shells[0].cwd = Some("/tmp/beta".into());
    app.tabs[2].shells[0].cwd = Some("/tmp/gamma-beta".into());
    for c in "beta".chars() {
        app.on_key(key(KeyCode::Char(c), KeyModifiers::NONE)).expect("type");
    }
    assert_eq!(app.search, "beta");
    assert_eq!(app.shown, vec![1, 2], "only matching tabs shown");
    app.on_key(key(KeyCode::Backspace, KeyModifiers::NONE)).expect("erase");
    assert_eq!(app.search, "bet");
    // The filtered list drives rendering and hit-testing.
    let buf = render(&mut app, W, H);
    assert!(row_text(&buf, 1, W).contains("⌕ bet█"), "typed filter with caret");
    assert!(row_text(&buf, 2, W).contains("beta"), "first shown tab is the match");
    assert_eq!(app.tab_at_row(2), Some(1), "hit-testing follows the filter");
    // Esc returns focus to the shell; typing no longer edits the filter.
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE)).expect("esc");
    assert!(!app.search_focus);
    app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE)).expect("shell key");
    assert_eq!(app.search, "bet", "unfocused typing goes to the shell");
    // Empty filter shows every tab again.
    app.search.clear();
    app.refresh_shown();
    assert_eq!(app.shown, vec![0, 1, 2]);
}

#[test]
fn search_focus_via_alt_f_mouse_and_tab_click() {
    // Kata: search row selectable by Ctrl+F and by mouse.
    let mut app = test_app(&[1, 1]);
    app.search_focus = false;
    app.on_key(key(KeyCode::Char('f'), KeyModifiers::ALT)).expect("alt+f");
    assert!(app.search_focus, "Alt+f focuses the search row");
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)).expect("enter");
    assert!(!app.search_focus, "Enter hands focus back");
    let down = |row| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 3,
        row,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(down(1)).expect("click search row");
    assert!(app.search_focus, "mouse click focuses the search row");
    app.on_mouse(down(2)).expect("click a tab");
    assert!(!app.search_focus, "selecting a tab unfocuses the search");
}

#[test]
fn search_hides_active_tab_without_breaking_reveal() {
    let mut app = test_app(&[1, 1]);
    app.tabs[0].shells[0].cwd = Some("/tmp/alpha".into());
    app.tabs[1].shells[0].cwd = Some("/tmp/beta".into());
    app.search = "alpha".into();
    app.refresh_shown();
    app.active = 1; // active tab filtered out
    app.sidebar_rows = 10;
    app.reveal_active(); // must not panic or scroll
    assert_eq!(app.list_offset, 0);
    assert_eq!(app.shown, vec![0]);
    let buf = render(&mut app, W, H);
    assert!(row_text(&buf, 2, W).contains("alpha"), "only the match renders");
}

// ── replay button (kata app.md) ──────────────────────────────────────────────

#[test]
fn replay_row_shows_icon_and_full_command_below_process() {
    // Kata tab: while a shell is the foreground process, the replay icon sits on
    // its own row below the process row, with the full command it will replay.
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].process = "bash".into();
    app.tabs[0].shells[0].last_cmd = Some("deno task dev".into());
    let buf = render(&mut app, W, H);
    // Row 4 is the process row (no icon now); row 5 is the new replay row.
    let process = row_text(&buf, 4, W);
    assert!(process.contains("└ bash"), "process row: {process:?}");
    assert!(!process.contains(REPLAY_LABEL), "icon left the process row: {process:?}");
    let replay = row_text(&buf, 5, W);
    // The icon renders two cells wide, so its continuation cell shows as a space
    // in extracted text; assert the icon and the full command, not exact gaps.
    assert!(
        replay.contains(REPLAY_LABEL) && replay.contains("deno task dev"),
        "icon + full command on the new row: {replay:?}"
    );
    let byte = replay.find(REPLAY_LABEL).expect("icon");
    let x = replay[..byte].chars().count() as u16; // column, not byte offset (multibyte gutter)
    let style = cell(&buf, x, 5).style();
    assert_eq!(style.fg, Some(REPLAY_COLOR), "icon carries the replay color");
    assert!(style.add_modifier.contains(Modifier::BOLD));
    // A non-shell program owns the tty → no replay row, even with a captured command.
    app.tabs[0].shells[0].process = "deno".into();
    let buf = render(&mut app, W, H);
    assert!((0..H).all(|y| !row_text(&buf, y, W).contains(REPLAY_LABEL)), "hidden while a program runs");
    // No last command → no replay row.
    app.tabs[0].shells[0].process = "bash".into();
    app.tabs[0].shells[0].last_cmd = None;
    let buf = render(&mut app, W, H);
    assert!((0..H).all(|y| !row_text(&buf, y, W).contains(REPLAY_LABEL)), "hidden without a last command");
}

#[test]
fn replay_row_grows_tab_and_tracks_hit_testing() {
    // Kata tab: the replay row adds a row while a shell is replayable, so the
    // tab height and the sidebar row hit-testing must track it.
    let mut app = test_app(&[1]);
    assert_eq!(app.tabs[0].rows(), 4, "no replay row before anything is captured");
    app.tabs[0].shells[0].process = "bash".into();
    app.tabs[0].shells[0].last_cmd = Some("cargo test".into());
    assert_eq!(app.tabs[0].rows(), 5, "a replay row grows the tab by one");
    // Rows: title, search, name(2), path(3), process(4), replay(5), separator(6).
    assert_eq!(app.shell_at_row(5), Some((0, 0)), "the replay row belongs to its shell");
    assert_eq!(app.replay_at(5, 5), Some((0, 0)), "the icon hit-tests on the replay row");
    assert_eq!(app.shell_at_row(6), Some((0, 0)), "the separator follows the replay row");
    // The button spans the icon through the command it re-runs: `{bar}    🔁 ` is
    // REPLAY_PAD (8) columns, then the 10 columns of `cargo test`.
    assert_eq!(app.replay_at(5, 17), Some((0, 0)), "the command's last column is still the button");
    assert_eq!(app.replay_at(5, 18), None, "past the command's end misses");
    // Losing replayability shrinks it back and removes the button.
    app.tabs[0].shells[0].last_cmd = None;
    assert_eq!(app.tabs[0].rows(), 4, "shrinks back when the command clears");
    assert!((0..8).all(|r| app.replay_at(r, 5).is_none()), "no button target once shrunk");
}

#[test]
fn replay_suppressed_while_non_shell_in_foreground() {
    // Kata app.md §94: replay (icon hit-test + Alt+r) is offered only while a
    // shell owns the tty, never for another program.
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].last_cmd = Some("echo hi".into());
    app.tabs[0].shells[0].process = "deno".into();
    // A program in the foreground → no replay row exists, so nothing hits.
    assert!((0..8).all(|r| app.replay_at(r, 5).is_none()), "no icon while a program runs");
    // A shell restores the replay row (below the process row) with its icon.
    app.tabs[0].shells[0].process = "bash".into();
    assert_eq!(app.replay_at(5, 5), Some((0, 0)), "icon back on the replay row at the shell prompt");
    assert_eq!(app.replay_at(4, 5), None, "the process row itself has no icon");
}

#[test]
fn replay_captures_typed_command_from_echo() {
    // Kata app.md §93: the last command typed and confirmed with Enter is what
    // replay re-runs — captured off the shell's echo, not /proc.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    thread::sleep(Duration::from_millis(200)); // let the prompt settle
    type_and_enter(&mut shell, "sleep 30");
    assert_eq!(shell.last_cmd.as_deref(), Some("sleep 30"), "typed command captured at Enter");
}

#[test]
fn replay_captures_builtin_the_proc_scan_would_miss() {
    // The old /proc approach could never see a shell builtin (no child process
    // to sample) nor a sub-500ms command; reading the echo catches both.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    thread::sleep(Duration::from_millis(200));
    type_and_enter(&mut shell, ": RICON_BUILTIN"); // `:` builtin: no process, instant
    // Not one sample of /proc could have caught it, yet it is replayable.
    shell.sample_proc();
    assert!(shell.fg_cmd.is_none(), "builtin left no foreground process");
    assert_eq!(shell.last_cmd.as_deref(), Some(": RICON_BUILTIN"), "builtin captured anyway");
}

#[test]
fn replay_captures_edited_line_as_executed() {
    // Reads the confirmed line off the echo, so mid-line edits land as the
    // command actually run, not the raw keys pressed.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    thread::sleep(Duration::from_millis(200));
    // Type ": AB", backspace the B, type C → the shell shows ": AC".
    for b in b": AB" {
        shell.note_input(&[*b]);
        shell.send(&[*b]);
    }
    assert!(wait_for(|| screen_contents(&shell).contains(": AB"), Duration::from_secs(10)), "typed");
    for b in [0x7f, b'C'] {
        shell.note_input(&[b]);
        shell.send(&[b]);
    }
    assert!(wait_for(|| screen_contents(&shell).contains(": AC"), Duration::from_secs(10)), "edited");
    shell.note_input(b"\r");
    shell.send(b"\r");
    assert_eq!(shell.last_cmd.as_deref(), Some(": AC"), "captured the edited line, not the keystrokes");
}

#[test]
fn replay_ignores_input_while_a_program_owns_the_tty() {
    // Kata app.md §93: capture only when the foreground is a shell. Keys sent
    // to a running program are not a command line and must not be captured.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    thread::sleep(Duration::from_millis(200));
    type_and_enter(&mut shell, "sleep 30");
    assert!(
        wait_for(
            || {
                shell.sample_proc();
                shell.process == "sleep"
            },
            Duration::from_secs(10)
        ),
        "program in foreground"
    );
    // Type into the running program: not a shell prompt, so last_cmd is frozen.
    for b in b"ignored keys" {
        shell.note_input(&[*b]);
        shell.send(&[*b]);
    }
    thread::sleep(Duration::from_millis(100));
    assert_eq!(shell.last_cmd.as_deref(), Some("sleep 30"), "input to a program is not captured");
    assert!(shell.cmd_anchor.is_none(), "no command line armed under a program");
}

#[test]
fn replay_types_and_confirms_via_alt_r_and_click() {
    // Kata: replay triggers (types and confirms) the command on click or Alt+r.
    let mut app = test_app(&[1]);
    assert!(
        wait_for(|| app.tabs[0].shells[0].activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)),
        "prompt"
    );
    thread::sleep(Duration::from_millis(200));
    app.tabs[0].shells[0].process = "bash".into(); // a shell owns the tty → replay armed
    app.tabs[0].shells[0].last_cmd = Some("echo RICON_REPLAY_$((1+1))".into());
    app.on_key(key(KeyCode::Char('r'), KeyModifiers::ALT)).expect("alt+r");
    assert!(
        wait_for(
            || screen_contents(&app.tabs[0].shells[0]).contains("RICON_REPLAY_2"),
            Duration::from_secs(10)
        ),
        "Alt+r typed and confirmed the command"
    );
    // Click on the button: the hit-test spans the icon (column 5, one space past
    // the process `└`) through the command text on the replay row (buffer row 5).
    app.tabs[0].shells[0].process = "bash".into();
    app.tabs[0].shells[0].last_cmd = Some("echo RICON_REPLAY_$((2+2))".into());
    assert_eq!(app.replay_at(5, 5), Some((0, 0)), "click lands on the icon");
    assert_eq!(app.replay_at(5, 4), None, "the indent before the icon misses");
    assert_eq!(app.replay_at(4, 5), None, "the process row has no icon");
    // The command is head-truncated to the sidebar, so it runs to the last column.
    assert_eq!(app.replay_at(5, app.sidebar_width - 2), Some((0, 0)), "the command text is the button too");
    assert_eq!(app.replay_at(5, app.sidebar_width - 1), None, "the border column is not");
    // A click on the command — not the icon — replays it.
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 12,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(click).expect("click replay");
    assert!(
        wait_for(
            || screen_contents(&app.tabs[0].shells[0]).contains("RICON_REPLAY_4"),
            Duration::from_secs(10)
        ),
        "mouse click replayed the command"
    );
}

// ── session transcripts ──────────────────────────────────────────────────────

/// Same env guard as `with_state_home`, for the transcript directory.
fn with_transcripts<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let saved = std::env::var_os("RICON_TRANSCRIPTS");
    unsafe { std::env::set_var("RICON_TRANSCRIPTS", dir) };
    let out = f();
    match saved {
        Some(v) => unsafe { std::env::set_var("RICON_TRANSCRIPTS", v) },
        None => unsafe { std::env::remove_var("RICON_TRANSCRIPTS") },
    }
    out
}

/// Every file written under `root`, sorted — transcripts land in a dated
/// subdirectory, so tests look for them rather than guessing the name.
fn written(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() { stack.push(path) } else { out.push(path) }
        }
    }
    out.sort();
    out
}

fn only_transcript(root: &Path) -> String {
    let files: Vec<PathBuf> = written(root).into_iter().filter(|p| p.ends_with_txt()).collect();
    assert_eq!(files.len(), 1, "exactly one transcript in {root:?}: {files:?}");
    std::fs::read_to_string(&files[0]).expect("read transcript")
}

/// `.tail.txt` sidecars are transcripts too as far as the filesystem is
/// concerned; this splits the two apart by suffix.
trait TranscriptPath {
    fn ends_with_txt(&self) -> bool;
    fn is_tail(&self) -> bool;
}

impl TranscriptPath for PathBuf {
    fn ends_with_txt(&self) -> bool {
        self.to_string_lossy().ends_with(".txt") && !self.is_tail()
    }
    fn is_tail(&self) -> bool {
        self.to_string_lossy().ends_with(".tail.txt")
    }
}

fn test_meta() -> transcript::Meta {
    transcript::Meta {
        agent: "claude".into(),
        model: "claude-opus-5".into(),
        cwd: PathBuf::from("/home/dev/code/gen/ricon"),
        pid: 4242,
    }
}

/// Feed `text` to a shell's screen and commit what that leaves ready, exactly
/// as the reader thread does after every chunk it parses.
fn feed(parser: &Mutex<vt100::Parser>, log: &Transcript, text: &str) {
    parser.lock().unwrap_or_else(PoisonError::into_inner).process(text.as_bytes());
    log.pump(parser, Pump::Output);
}

/// What the session's flush thread does once a second.
fn flush_pump(log: &Transcript, parser: &Mutex<vt100::Parser>) {
    log.pump(parser, Pump::Flush);
}

fn test_parser(rows: u16, cols: u16, scrollback: usize) -> Mutex<vt100::Parser> {
    Mutex::new(vt100::Parser::new(rows, cols, scrollback))
}

#[test]
fn stamp_reads_a_unix_second_as_civil_date_and_time() {
    assert_eq!(transcript::stamp(0), stamp_of(1970, 1, 1, 0, 0, 0), "the epoch itself");
    assert_eq!(transcript::stamp(1_771_632_896), stamp_of(2026, 2, 21, 0, 14, 56), "an ordinary moment");
    // A leap day, the day after it, and a century that is not a leap year.
    assert_eq!(transcript::stamp(1_709_209_800), stamp_of(2024, 2, 29, 12, 30, 0), "leap day");
    assert_eq!(transcript::stamp(1_709_296_200), stamp_of(2024, 3, 1, 12, 30, 0), "the day after");
    assert_eq!(transcript::stamp(4_107_542_400), stamp_of(2100, 3, 1, 0, 0, 0), "1900-rule century");
    // Before the epoch the arithmetic must floor, not truncate towards zero.
    assert_eq!(transcript::stamp(-1), stamp_of(1969, 12, 31, 23, 59, 59), "one second before");
}

fn stamp_of(year: i64, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> transcript::Stamp {
    transcript::Stamp { year, month, day, hour, min, sec }
}

#[test]
fn stamp_formats_the_dated_file_name_and_its_markers() {
    let at = transcript::stamp(1_771_632_896);
    assert_eq!(at.date(), "2026-02-21", "the dated directory");
    assert_eq!(at.time(), "00:14:56", "the marker inside the file");
    assert_eq!(at.compact(), "001456", "the file name carries no colons");
    assert_eq!(at.full(), "2026-02-21 00:14:56", "the header");
}

#[test]
fn transcript_dir_follows_the_override_and_can_be_switched_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        assert_eq!(transcript::transcript_dir().as_deref(), Some(dir.path()), "override wins");
    });
    for off in ["off", "0", "no", "false", ""] {
        with_transcripts(Path::new(off), || {
            assert_eq!(transcript::transcript_dir(), None, "`{off}` writes nothing");
        });
    }
}

#[test]
fn transcript_commits_lines_as_they_scroll_off_the_screen() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let log = Transcript::new(100);
        log.arm(test_meta());
        assert!(log.armed(), "an agent was detected in this shell");
        let parser = test_parser(4, 40, 100);
        // Four rows of screen: the first lines are pushed into the scrollback,
        // which is what a transcript is made of.
        feed(&parser, &log, "alpha\r\nbeta\r\ngamma\r\ndelta\r\nepsilon\r\n");
        let text = only_transcript(dir.path());
        assert!(text.contains("claude (claude-opus-5)"), "the header names the session: {text}");
        assert!(text.contains("alpha"), "the oldest line is committed: {text}");
        assert!(text.contains("beta"), "and the ones behind it: {text}");
        // `epsilon` is still on screen, so it belongs to the sidecar, not the
        // transcript — until the session is closed.
        assert!(!text.contains("epsilon"), "the live screen is not committed twice: {text}");
        flush_pump(&log, &parser); // the sidecar is the flush thread's
        let tails: Vec<PathBuf> = written(dir.path()).into_iter().filter(PathBuf::is_tail).collect();
        assert_eq!(tails.len(), 1, "the uncommitted screen is mirrored beside it");
        let tail = std::fs::read_to_string(&tails[0]).expect("read sidecar");
        assert!(tail.contains("epsilon"), "a power cut still finds the newest line: {tail}");

        log.close(&parser);
        let text = only_transcript(dir.path());
        assert!(text.contains("epsilon"), "closing folds the last screen in: {text}");
        assert!(!log.armed(), "a closed transcript takes no more writes");
        assert!(written(dir.path()).iter().all(|p| !p.is_tail()), "and the sidecar is gone");
    });
}

#[test]
fn transcript_never_commits_the_same_line_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let log = Transcript::new(100);
        log.arm(test_meta());
        let parser = test_parser(4, 40, 100);
        feed(&parser, &log, "alpha\r\nbeta\r\ngamma\r\ndelta\r\nepsilon\r\n");
        // Quiet passes (the flush thread's) and further output must not
        // re-commit what is already in.
        feed(&parser, &log, "");
        flush_pump(&log, &parser);
        feed(&parser, &log, "zeta\r\n");
        log.close(&parser);
        let text = only_transcript(dir.path());
        assert_eq!(text.matches("alpha").count(), 1, "committed once: {text}");
        assert_eq!(text.matches("epsilon").count(), 1, "even as it scrolls off later: {text}");
        assert_eq!(text.matches("zeta").count(), 1, "and the last screen is folded in once: {text}");
    });
}

#[test]
fn transcript_starts_at_the_tail_of_history_that_predates_the_agent() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let parser = test_parser(4, 40, 5000);
        // A shell with a long life behind it: a build log, not a session.
        let old: String = (0..900).map(|n| format!("old-{n}\r\n")).collect();
        let log = Transcript::new(5000);
        feed(&parser, &log, &old);
        log.arm(test_meta());
        feed(&parser, &log, "hello agent\r\n");
        log.close(&parser);
        let text = only_transcript(dir.path());
        assert!(!text.contains("old-1\n"), "history far behind the agent is left out");
        assert!(text.contains("old-899"), "the lines around it are context: {text}");
        assert!(text.contains("hello agent"), "and the session itself is in: {text}");
    });
}

#[test]
fn transcript_snapshots_the_alternate_screen_where_nothing_scrolls_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let log = Transcript::new(100);
        log.arm(test_meta());
        let parser = test_parser(6, 40, 100);
        // A full-screen client (opencode): its chat scrolls inside the alternate
        // screen, so the screen itself is the only record there is. It repaints
        // in several writes, and the half-drawn screen between them is not what
        // the user is reading — only a settled frame is committed.
        feed(&parser, &log, "\x1b[?1049h\x1b[H\x1b[2J opencode\r\n");
        flush_pump(&log, &parser);
        feed(&parser, &log, "> what does this do?\r\nit reads the code");
        let committed = written(dir.path())
            .iter()
            .filter(|p| p.ends_with_txt())
            .any(|p| std::fs::read_to_string(p).is_ok_and(|text| text.contains("it reads the code")));
        assert!(!committed, "a frame still being drawn is never committed");
        thread::sleep(Duration::from_millis(500));
        flush_pump(&log, &parser); // the console went quiet: the frame is finished
        let text = only_transcript(dir.path());
        assert!(text.contains("what does this do?"), "the screen is snapshotted: {text}");
        assert!(text.contains("it reads the code"), "all of it: {text}");
        let tails: Vec<PathBuf> = written(dir.path()).into_iter().filter(PathBuf::is_tail).collect();
        assert_eq!(tails.len(), 1, "a crash mid-stream still leaves the live frame beside it");
        log.close(&parser);
        let text = only_transcript(dir.path());
        assert_eq!(
            text.matches("it reads the code").count(),
            1,
            "the last snapshot is not folded in twice: {text}"
        );
        assert!(written(dir.path()).iter().all(|p| !p.is_tail()), "and the sidecar is gone");
    });
}

#[test]
fn transcript_capture_leaves_the_users_scroll_position_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let log = Transcript::new(100);
        log.arm(test_meta());
        let parser = test_parser(4, 40, 100);
        feed(&parser, &log, "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n");
        parser.lock().expect("parser").screen_mut().set_scrollback(2); // the user scrolled back to read
        flush_pump(&log, &parser);
        let view = parser.lock().expect("parser").screen().scrollback();
        assert_eq!(view, 2, "the view is put back before the lock is released");
    });
}

#[test]
fn no_transcript_is_written_for_a_shell_with_no_agent() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let log = Transcript::new(100);
        assert!(!log.armed(), "nothing is transcribed until a client is detected");
        let parser = test_parser(4, 40, 100);
        feed(&parser, &log, "secret\r\nlines\r\nin\r\na\r\nplain\r\nshell\r\n");
        log.close(&parser);
        assert!(written(dir.path()).is_empty(), "no file at all: {:?}", written(dir.path()));
    });
}

#[test]
fn shell_transcribes_its_console_and_closes_the_file_when_it_goes_away() {
    let home = tempfile::tempdir().expect("tempdir");
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let mut shell = test_shell(home.path());
        assert!(
            wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)),
            "shell prompt"
        );
        // What `tick_agent` does the moment the probe reports a client.
        shell.agent = Some(AgentInfo {
            name: "claude",
            model: "claude-opus-5".into(),
            pid: 1,
            context: None,
            status: None,
        });
        shell.arm_log();
        assert!(shell.log.armed(), "the shell is being transcribed");
        // Enough lines to push the first ones off a 24-row screen.
        shell.send_line("for i in $(seq 1 40); do echo RICON_LOG_$i; done");
        assert!(
            wait_for(|| screen_contents(&shell).contains("RICON_LOG_40"), Duration::from_secs(10)),
            "the command ran"
        );
        assert!(
            wait_for(|| written(dir.path()).iter().any(|p| p.ends_with_txt()), Duration::from_secs(5)),
            "a transcript was opened"
        );
        drop(shell); // closing the tab folds the last screen in
        let text = only_transcript(dir.path());
        assert!(text.contains("RICON_LOG_1\n"), "the scrolled-off output is in: {text}");
        assert!(text.contains("RICON_LOG_40"), "and the screen it never scrolled off: {text}");
        assert!(written(dir.path()).iter().all(|p| !p.is_tail()), "the sidecar is folded in and gone");
    });
}

// ── audit regressions ────────────────────────────────────────────────────────

#[test]
fn transcript_keeps_writing_after_the_scrollback_is_full() {
    // vt100 caps the scrollback and drops its oldest line for each new one, so
    // its length stops growing: a transcript counting lines by it went silent
    // for good once a long session filled the buffer.
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let (cap, rows) = (40, 4);
        let log = Transcript::new(cap);
        assert!(log.arm(test_meta()).is_some(), "a session starts");
        let parser = test_parser(rows, 40, cap);
        // Chunks of every size up to the point the anchor itself would be
        // pushed out, crossing the cap many times over.
        let mut n = 0;
        for size in (1..=cap - 16).cycle().take(30) {
            let chunk: String = (n..n + size).map(|i| format!("line-{i:05}\r\n")).collect();
            feed(&parser, &log, &chunk);
            n += size;
        }
        log.close(&parser);
        let text = only_transcript(dir.path());
        let got: Vec<usize> =
            text.lines().filter_map(|l| l.strip_prefix("line-")).filter_map(|l| l.parse().ok()).collect();
        let want: Vec<usize> = (0..n).collect();
        assert_eq!(got, want, "every line exactly once, in order");
    });
}

#[test]
fn transcript_close_commits_what_scrolled_off_since_the_last_pass() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let log = Transcript::new(100);
        log.arm(test_meta());
        let parser = test_parser(4, 40, 100);
        feed(&parser, &log, "first\r\n");
        // Parsed, but the reader thread never got to pump it: the tab closed.
        parser.lock().expect("parser").process(b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n");
        log.close(&parser);
        let text = only_transcript(dir.path());
        for line in ["first", "a", "b", "f"] {
            assert!(text.lines().any(|l| l == line), "{line:?} is in: {text}");
        }
    });
}

#[test]
fn transcript_ends_with_its_agent_and_the_next_one_gets_a_file_of_its_own() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_transcripts(dir.path(), || {
        let log = Transcript::new(100);
        let parser = test_parser(4, 40, 100);
        let first = log.arm(test_meta()).expect("first session");
        assert!(log.serves(first));
        feed(&parser, &log, "session-one\r\n1\r\n2\r\n3\r\n4\r\n");
        assert_eq!(log.arm(test_meta()), None, "one session at a time");
        log.close(&parser);
        assert!(!log.serves(first), "its flush thread retires");
        // The agent is gone: the shell's own output is nobody's transcript.
        feed(&parser, &log, "private\r\nshell\r\nwork\r\nhere\r\n");
        thread::sleep(Duration::from_millis(1100)); // a file name per second
        let second = log.arm(test_meta()).expect("second session");
        assert_ne!(first, second);
        feed(&parser, &log, "session-two\r\nx\r\ny\r\nz\r\nw\r\n");
        log.close(&parser);
        let files: Vec<PathBuf> = written(dir.path()).into_iter().filter(|p| p.ends_with_txt()).collect();
        assert_eq!(files.len(), 2, "a file per session: {files:?}");
        let texts: Vec<String> = files.iter().map(|f| std::fs::read_to_string(f).expect("read")).collect();
        let one = texts.iter().find(|t| t.contains("session-one")).expect("first file");
        let two = texts.iter().find(|t| t.contains("session-two")).expect("second file");
        assert!(!two.contains("session-one"), "the first session is not written again: {two}");
        assert!(!one.contains("session-two"), "nor the second into the first: {one}");
    });
    with_transcripts(Path::new("off"), || {
        assert_eq!(Transcript::new(100).arm(test_meta()), None, "transcripts off: nothing starts");
    });
}

#[test]
fn ctrl_symbols_send_the_bytes_a_terminal_sends() {
    // crossterm reports 0x1c..0x1f as Ctrl+4..7 and 0x00 as Ctrl+Space.
    let ctrl = |c| encode_key(&key(KeyCode::Char(c), KeyModifiers::CONTROL), false);
    assert_eq!(ctrl('4'), Some(vec![0x1c]), "Ctrl+\\ is SIGQUIT, not Ctrl+T");
    assert_eq!(ctrl('5'), Some(vec![0x1d]), "Ctrl+]");
    assert_eq!(ctrl('7'), Some(vec![0x1f]), "Ctrl+_");
    assert_eq!(ctrl(' '), Some(vec![0x00]), "Ctrl+Space");
    assert_eq!(ctrl('c'), Some(vec![0x03]), "letters keep their low bits");
    assert_eq!(ctrl('?'), Some(vec![0x7f]));
}

#[test]
fn ctrl_alt_chords_reach_the_app() {
    let mut app = test_app(&[1]);
    app.search_focus = false;
    app.on_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL | KeyModifiers::ALT)).expect("C-M-t");
    assert_eq!(app.tabs.len(), 1, "Ctrl+Alt+t is the app's, not a new tab");
    app.on_key(key(KeyCode::Char('t'), KeyModifiers::ALT)).expect("M-t");
    assert_eq!(app.tabs.len(), 2, "a bare Alt+t still is");
}

#[test]
fn pasted_text_cannot_break_out_of_its_bracket() {
    assert_eq!(paste_text("a\nb\r\nc"), "a\rb\rc", "line breaks are typed as Enter");
    assert_eq!(paste_text("x\x1b[201~rm -rf ~\x1b[200~y"), "xrm -rf ~y", "the markers are removed");
}

#[test]
fn block_selection_is_the_same_rectangle_from_either_diagonal() {
    let cells = |a, b| block_cells(a, b).collect::<Vec<_>>();
    let want = cells((1, 2), (3, 4));
    assert_eq!(want.len(), 9);
    assert_eq!(cells((1, 4), (3, 2)), want, "top-right to bottom-left");
    assert_eq!(cells((3, 2), (1, 4)), want, "bottom-left to top-right");
}

#[test]
fn json_string_reads_only_string_values_of_keys() {
    assert_eq!(json_string("{\"model\": \"opus\"}", "model").as_deref(), Some("opus"));
    assert_eq!(json_string("{\"model\": null, \"foo\":\"bar\"}", "model"), None, "null is no model");
    assert_eq!(
        json_string("{\"name\":\"model\",\"model\":\"x\"}", "model").as_deref(),
        Some("x"),
        "keys only"
    );
}

#[test]
fn session_usage_is_found_behind_a_huge_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("s.jsonl");
    let answer = "{\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":7}}}\n";
    let tool = format!("{{\"toolUseResult\":\"{}\"}}\n", "x".repeat(300 * 1024));
    std::fs::write(&file, format!("{answer}{tool}")).expect("write");
    assert_eq!(scan_tail(&file, last_session_usage), Some(7), "past the first 64 KiB");
    assert_eq!(scan_tail(&file, |_: &str| None::<u64>), None, "and it stops at the file's start");
}

#[test]
fn opencode_context_limit_is_the_providers_own_entry() {
    let home = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(home.path().join(".cache/opencode")).expect("cache dir");
    // An aggregator lists the same model first, with another id and limit.
    let catalog = r#"{"router":{"models":{"anthropic/claude-x":{"id":"anthropic/claude-x","limit":{"context":200000}}}},
        "anthropic":{"models":{"claude-x":{"id":"claude-x","limit":{"context":1000000}},"claude-y":{"id":"claude-y"}}}}"#;
    std::fs::write(home.path().join(".cache/opencode/models.json"), catalog).expect("catalog");
    with_home(home.path(), || {
        assert_eq!(opencode_context_limit("anthropic", "claude-x"), Some(1_000_000));
        assert_eq!(opencode_context_limit("anthropic", "claude-y"), None, "no limit: not the next model's");
        assert_eq!(opencode_context_limit("ollama", "claude-x"), None, "not in the catalog");
    });
}

#[test]
fn a_stale_idle_status_does_not_outlive_the_users_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut shell = test_shell(dir.path());
    let a_minute = SystemTime::now() - Duration::from_secs(60);
    shell.agent = Some(AgentInfo {
        name: "claude",
        model: "m".into(),
        pid: 1,
        context: None,
        status: Some(Status::Idle(a_minute)),
    });
    shell.last_input = Instant::now() - IDLE_NUDGE * 2;
    assert_eq!(shell.status(), Some(Status::Idle(a_minute)), "idle since after the last keystroke");
    shell.last_input = Instant::now();
    assert_eq!(shell.status(), None, "read before the Enter just sent: not trusted");
}

#[test]
fn the_continue_step_never_lands_in_what_the_user_is_typing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut shell = test_shell(dir.path());
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    let shell_pid = shell.pid.expect("shell pid");
    type_and_enter(&mut shell, "cat > /dev/null");
    assert!(wait_for(|| !shell.shell_fg(), Duration::from_secs(10)), "a program owns the tty");
    let pid = foreground_pid(shell_pid).expect("foreground program");
    shell.agent = Some(AgentInfo { name: "claude", model: "m".into(), pid, context: None, status: None });
    let at = Instant::now() - COMPACT_TIMEOUT;
    shell.nudge = Some(Nudge::Compacting(at));
    shell.last_content_change = at;
    shell.last_input = Instant::now(); // the user came back mid-compaction
    shell.nudge_if_idle();
    assert!(shell.nudge.is_none(), "the nudge is abandoned");
    thread::sleep(Duration::from_millis(200));
    assert!(!screen_contents(&shell).contains(DEFAULT_CONTINUE), "nothing typed into the draft");
}

#[test]
fn closing_an_earlier_tab_keeps_focus_on_the_same_tab() {
    let mut app = test_app(&[1, 1, 1]);
    app.active = 2;
    let focused = app.tabs[2].shells[0].pid;
    let _ = app.tabs[0].shells[0].child.kill();
    assert!(
        wait_for(|| !matches!(app.tabs[0].shells[0].child.try_wait(), Ok(None)), Duration::from_secs(10)),
        "the first tab's shell exits"
    );
    app.reap_dead_tabs();
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.tabs[app.active].shells[0].pid, focused, "focus stays where the user was");
}

#[test]
fn closing_the_cheat_sheet_swallows_the_rest_of_its_click() {
    let mut app = test_app(&[1]);
    app.help = true;
    let at = |kind| MouseEvent { kind, column: 40, row: 5, modifiers: KeyModifiers::NONE };
    app.on_mouse(at(MouseEventKind::Down(MouseButton::Left))).expect("press");
    assert!(!app.help && app.swallow_release, "closed, and the release is ricon's");
    app.on_mouse(at(MouseEventKind::Up(MouseButton::Left))).expect("release");
    assert!(!app.swallow_release, "swallowed once, then the mouse is the app's again");
}
