//! Full behavioral test suite. Every user-visible rule from `.katana/` has a
//! test here: pure helpers, theming, key/mouse encoding, session persistence,
//! sidebar layout math, live-PTY end-to-end behavior, and UI rendering
//! (asserted on real `TestBackend` buffers, colors and modifiers included).

use super::*;
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Modifier};
use std::sync::Mutex;

// ── harness ──────────────────────────────────────────────────────────────────

/// Serializes tests that mutate process-global env (only `XDG_STATE_HOME` is
/// ever mutated, and only session tests read it — nothing else races).
static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        shell.writer.write_all(&[b]).expect("write");
        shell.writer.flush().expect("flush");
    }
    assert!(
        wait_for(|| screen_contents(shell).contains(cmd), Duration::from_secs(10)),
        "typed line echoed, got: {:?}",
        screen_contents(shell)
    );
    shell.note_input(b"\r");
    shell.writer.write_all(b"\r").expect("write");
    shell.writer.flush().expect("flush");
}

fn screen_contents(shell: &Shell) -> String {
    shell.parser.lock().unwrap_or_else(PoisonError::into_inner).screen().contents()
}

/// An `App` with one tab per entry, each holding that many shells; colors and
/// cwds follow the production path (`tab_color(i)`, current dir). Like
/// `App::new`, the search row starts focused and `shown` is pre-filtered.
fn test_app(shell_counts: &[usize]) -> App {
    let base = std::env::current_dir().expect("cwd");
    let mut tabs = Vec::new();
    for (i, &n) in shell_counts.iter().enumerate() {
        let mut tab = Tab::spawn(24, 80, &base, tab_color(i), None).expect("spawn tab");
        for _ in 1..n {
            tab.shells.push(test_shell(&base));
        }
        tabs.push(tab);
    }
    let mut app = App {
        tabs,
        active: 0,
        created: shell_counts.len(),
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
        quit: false,
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
    // Single row: an inclusive column span.
    assert_eq!(selection_cells((2, 3), (2, 5), 10), [(2, 3), (2, 4), (2, 5)]);
    // Multi row: tail of the first row, full middle rows, head of the last.
    assert_eq!(selection_cells((0, 2), (2, 1), 3), [(0, 2), (1, 0), (1, 1), (1, 2), (2, 0), (2, 1)]);
}

#[test]
fn order_normalizes_into_reading_order() {
    assert_eq!(order((5, 0), (1, 9)), ((1, 9), (5, 0)));
    assert_eq!(order((2, 7), (2, 3)), ((2, 3), (2, 7)));
}

#[test]
fn truncate_tail_elides_head_with_ellipsis() {
    // width 8, pad 5 → 3 columns → "…" plus the last two chars.
    assert_eq!(truncate_tail("abcdef", 8, 5), "…def");
    assert!(truncate_tail("abcdef", 8, 5).chars().count() <= 4);
}

#[test]
fn truncate_tail_is_char_safe_on_multibyte() {
    assert_eq!(truncate_tail("ééééééé", 8, 5), "…ééé");
}

#[test]
fn expand_home_handles_tilde_forms() {
    let home = std::env::var("HOME").expect("HOME set");
    assert_eq!(expand_home("~"), PathBuf::from(&home));
    assert_eq!(expand_home("~/x"), PathBuf::from(format!("{home}/x")));
    assert_eq!(expand_home("~user/x"), PathBuf::from("~user/x"));
    assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
}

#[test]
fn abbreviate_home_shortens_only_home_prefix() {
    let home = std::env::var("HOME").expect("HOME set");
    assert_eq!(abbreviate_home(&format!("{home}/code")), "~/code");
    assert_eq!(abbreviate_home("/usr/lib"), "/usr/lib");
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
fn version_is_0_3_0() {
    // Kata meta.md: ricon app version is 0.3.0.
    assert_eq!(env!("CARGO_PKG_VERSION"), "0.3.0");
}

// ── theming ──────────────────────────────────────────────────────────────────

#[test]
fn hsl_rgb_hits_the_primary_anchors() {
    assert_eq!(hsl_rgb(0.0, 1.0, 0.5), Color::Rgb(255, 0, 0));
    assert_eq!(hsl_rgb(120.0, 1.0, 0.5), Color::Rgb(0, 255, 0));
    assert_eq!(hsl_rgb(240.0, 1.0, 0.5), Color::Rgb(0, 0, 255));
    assert_eq!(hsl_rgb(0.0, 0.0, 0.0), Color::Rgb(0, 0, 0));
    assert_eq!(hsl_rgb(0.0, 0.0, 1.0), Color::Rgb(255, 255, 255));
}

#[test]
fn tab_colors_are_distinct_for_24_tabs() {
    // Kata app.md: every terminal tab has different aesthetical colors —
    // 8 hues × 3 lightness cycles = 24 unique colors before repeating.
    let colors: Vec<Color> = (0..24).map(tab_color).collect();
    for (i, a) in colors.iter().enumerate() {
        for (j, b) in colors.iter().enumerate() {
            assert!(i == j || a != b, "tab colors {i} and {j} collide: {a:?}");
        }
    }
}

#[test]
fn tab_colors_avoid_warm_hues() {
    // The palette is cool-biased (azure→violet→teal→indigo): every hue stays
    // in the green→blue→magenta arc — no reds, oranges or yellows.
    for n in 0..24 {
        let Color::Rgb(r, g, b) = tab_color(n) else { panic!("tab_color must be RGB") };
        let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let hue = if max == g {
            60.0 * (2.0 + (b - r) / (max - min))
        } else if max == b {
            60.0 * (4.0 + (r - g) / (max - min))
        } else {
            (60.0 * (6.0 + (g - b) / (max - min))) % 360.0
        };
        assert!((140.0..=320.0).contains(&hue), "tab {n} hue {hue} is warm: {:?}", tab_color(n));
    }
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
        },
        TabState {
            shells: vec![ShellState { cwd: "/tmp/c".into(), cmd: None }],
            active_shell: 0,
            active: true,
            favorite: false,
        },
    ]
}

#[test]
fn session_roundtrip_preserves_everything() {
    // Kata app.md: tabs, folders, commands, favorites, active tab and active
    // shell are all persisted and restored.
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let states = sample_states();
        save_session(&states);
        assert_eq!(load_session(), states);
    });
}

#[test]
fn session_empty_and_missing_files_load_as_no_tabs() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        assert_eq!(load_session(), Vec::new());
        save_session(&[]);
        assert_eq!(load_session(), Vec::new());
    });
}

#[test]
fn session_markers_parse_in_any_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let path = session_path().expect("path");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, ">!*/tmp/x\tmake -j\n+/tmp/y\n").expect("write");
        let tabs = load_session();
        assert_eq!(tabs.len(), 1);
        assert!(tabs[0].active && tabs[0].favorite);
        assert_eq!(tabs[0].active_shell, 0);
        assert_eq!(tabs[0].shells[0].cmd.as_deref(), Some("make -j"));
        assert_eq!(tabs[0].shells[1].cwd, PathBuf::from("/tmp/y"));
    });
}

#[test]
fn persist_now_writes_once_per_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    with_state_home(dir.path(), || {
        let mut app = test_app(&[1]);
        app.persist_now();
        let first = app.saved_session.clone();
        assert_eq!(first.len(), 1);
        assert!(first[0].active);
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
    };
    app.restore_tab(&state).expect("restore");
    let tab = app.tabs.last().expect("restored tab");
    assert_eq!(tab.shells.len(), 2);
    assert_eq!(tab.active, 1);
    assert!(tab.favorite);
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
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    shell.writer.write_all(b"echo RICON_$((40+2))\r").expect("write");
    shell.writer.flush().expect("flush");
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
    let sel = app.selection.expect("selection survives release");
    assert!(!sel.dragging, "release ends the drag");
    assert_eq!(app.selection_text(sel).as_deref(), Some(tok));

    // Rendered: exactly the token's cells are reversed, nothing past it.
    let buf = render(&mut app, sw + 80, 25);
    for c in 0..tok.len() as u16 {
        assert!(cell(&buf, sw + col + c, row).modifier.contains(Modifier::REVERSED), "cell {c} reversed");
    }
    assert!(
        !cell(&buf, sw + col + tok.len() as u16, row).modifier.contains(Modifier::REVERSED),
        "cell past the token is untouched"
    );

    // A plain click (press+release, no movement) deselects.
    app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), col)).expect("click");
    app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), col)).expect("release click");
    assert!(app.selection.is_none(), "click deselects");
}

#[test]
fn shell_reports_foreground_process_and_cmdline() {
    // Kata app.md: third row shows the running process; commands persist.
    let dir = std::env::current_dir().expect("cwd");
    let mut shell = test_shell(&dir);
    assert!(wait_for(|| shell.activity.load(Ordering::Relaxed) > 0, Duration::from_secs(10)), "prompt");
    thread::sleep(Duration::from_millis(200)); // let the prompt settle
    shell.writer.write_all(b"sleep 30\r").expect("write");
    shell.writer.flush().expect("flush");
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
    app.write_active(b"").expect("write"); // any input snaps back to live
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
    let (c0, c1) = (app.tabs[0].color, app.tabs[1].color);
    app.toggle_favorite(); // favorite tab 0 (stays at index 0)
    app.open_tab().expect("open");
    assert_eq!(app.tabs.len(), 3);
    assert_eq!(app.active, 1, "new tab lands after the favorites block");
    assert_eq!(app.tabs[0].color, c0);
    assert_eq!(app.tabs[2].color, c1);
    // The new tab inherits the active tab's directory (the base here).
    assert_eq!(app.tabs[1].shells[0].cwd.as_deref(), Some(app.base.as_path()));
    // And its color is distinct from both existing tabs.
    assert!(app.tabs[1].color != c0 && app.tabs[1].color != c1);
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
    let (a, b, c) = (app.tabs[0].color, app.tabs[1].color, app.tabs[2].color);
    app.active = 2;
    app.toggle_favorite(); // C → favorite, moves to top
    assert_eq!((app.tabs[0].color, app.active), (c, 0));
    app.active = 2;
    app.toggle_favorite(); // B → favorite, lands after C
    assert_eq!(app.tabs[1].color, b);
    assert_eq!(app.tabs[2].color, a);
    assert!(app.tabs[0].favorite && app.tabs[1].favorite && !app.tabs[2].favorite);
    app.toggle_favorite(); // unmark B → drops just below the favorites block
    assert!(!app.tabs[1].favorite);
    assert_eq!(app.tabs[1].color, b);
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
    let buf = render(&mut app, W, H);
    let path_row = row_text(&buf, 3, W);
    let expected = abbreviate_home(&app.base.display().to_string());
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
    // The `*` sits after the name, inside the sidebar (border excluded).
    let second = row_text(&buf, 6, SIDEBAR_WIDTH - 1);
    assert!(second.trim_end().ends_with('*'), "unseen marker: {second:?}");
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
    // Kata app.md: white braille spinner after the process while active.
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].process = "cargo".into();
    app.tabs[0].shells[0].animating = true;
    let buf = render(&mut app, W, H);
    let spin_x = (0..W)
        .find(|&x| matches!(cell(&buf, x, 4).symbol().chars().next(), Some('\u{2800}'..='\u{28FF}')))
        .expect("braille spinner visible on the process row");
    let style = cell(&buf, spin_x, 4).style();
    assert_eq!(style.fg, Some(SPINNER_COLOR), "spinner is white");
    assert!(style.add_modifier.contains(Modifier::BOLD));
    // Without activity the spinner disappears.
    app.tabs[0].shells[0].animating = false;
    let buf = render(&mut app, W, H);
    assert!(
        !(0..W).any(|x| matches!(cell(&buf, x, 4).symbol().chars().next(), Some('\u{2800}'..='\u{28FF}'))),
        "spinner removed after settle"
    );
}

#[test]
fn render_theming_gives_each_tab_its_own_color() {
    // Kata app.md: every terminal tab has different aesthetical colors — the
    // whole row (and the footer for the active tab) carries the tab's color.
    let mut app = test_app(&[1, 1, 1]);
    let buf = render(&mut app, W, H);
    for (i, y) in [(0usize, 2u16), (1, 6), (2, 10)] {
        assert_eq!(cell(&buf, 1, y).style().bg, Some(tab_color(i)), "tab {i} background");
        assert_eq!(cell(&buf, 1, y).style().fg, Some(Color::White), "tab {i} foreground");
    }
    assert_eq!(cell(&buf, 1, H - 1).style().bg, Some(tab_color(0)), "footer matches active tab");
    app.active = 1;
    let buf = render(&mut app, W, H);
    assert_eq!(cell(&buf, 1, H - 1).style().bg, Some(tab_color(1)), "footer follows active tab");
}

#[test]
fn render_footer_shows_index_path_branch_and_version() {
    // Kata app.md/meta.md: footer shows `index/count`, path, git branch, and
    // the version pinned to the right corner.
    let mut app = test_app(&[1, 1]);
    app.active = 1;
    let buf = render(&mut app, W, H);
    let footer = row_text(&buf, H - 1, W);
    assert!(footer.contains("2/2"), "index/count: {footer:?}");
    let path = abbreviate_home(&app.base.display().to_string());
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
fn status_bar_truncates_left_but_pins_version_right() {
    let app = test_app(&[1]);
    let shell = app.tabs[0].active_shell();
    let line = status_bar(0, 9, shell, Some("main".into()), Color::Blue, 30, true);
    let text = line_text(&line);
    assert_eq!(text.chars().count(), 30, "line exactly fills the width");
    assert!(text.starts_with(" ↕ 1/9"), "indicator and index: {text:?}");
    assert!(text.ends_with(concat!("v", env!("CARGO_PKG_VERSION"), " ")), "version survives: {text:?}");
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
    app.on_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL)).expect("ctrl+q");
    assert!(app.quit, "Ctrl+q quits gracefully");
}

#[test]
fn shortcuts_navigate_shells_within_tab() {
    let mut app = test_app(&[2]);
    app.on_key(key(KeyCode::Down, KeyModifiers::ALT)).expect("alt+down");
    assert_eq!(app.tabs[0].active, 1);
    app.on_key(key(KeyCode::Up, KeyModifiers::ALT)).expect("alt+up");
    assert_eq!(app.tabs[0].active, 0);
    app.on_key(key(KeyCode::Char('f'), KeyModifiers::ALT)).expect("alt+f");
    assert!(app.tabs[0].favorite, "Alt+f toggles favorite");
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
    let (c0, c1) = (app.tabs[0].color, app.tabs[1].color);
    let event = |kind, row| MouseEvent { kind, column: 3, row, modifiers: KeyModifiers::NONE };
    app.on_mouse(event(MouseEventKind::Down(MouseButton::Left), 2)).expect("grab tab 0");
    app.on_mouse(event(MouseEventKind::Drag(MouseButton::Left), 6)).expect("drag to tab 1");
    app.on_mouse(event(MouseEventKind::Up(MouseButton::Left), 6)).expect("drop");
    assert_eq!((app.tabs[0].color, app.tabs[1].color), (c1, c0), "tabs swapped");
    assert_eq!(app.active, 1, "selection follows the dragged tab");
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
    assert_eq!(app.visible_tabs(), 0..2, "tab 2 is fully below the fold");
    app.list_offset = 1;
    assert_eq!(app.visible_tabs(), 1..3);
    app.sidebar_rows = 0; // degenerate sidebar renders nothing
    assert_eq!(app.visible_tabs(), 1..1);
}

#[test]
fn agent_scan_runs_only_for_the_on_screen_shell() {
    // Agent detection walks all of /proc — O(system processes). The per-shell
    // 2-Hz sweep must never trigger it; only `tick_agent` on the shell whose
    // status bar is visible may, and a plain shell resolves to no agent.
    let mut app = test_app(&[1, 1]);
    for tab in &mut app.tabs {
        for shell in &mut tab.shells {
            shell.sample_proc(); // cwd/process/cmdline only — no /proc-wide scan
        }
    }
    assert!(app.tabs.iter().all(|t| t.shells.iter().all(|s| s.agent.is_none())));
    let shell = app.tabs[0].active_shell_mut();
    shell.agent_sampled = Instant::now() - SAMPLE_EVERY * 2;
    shell.tick_agent();
    assert!(shell.agent.is_none(), "no AI agent under a bare shell");
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
        let saved = &app.saved_session[0].shells[0];
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
fn search_focus_via_ctrl_f_mouse_and_tab_click() {
    // Kata: search row selectable by Ctrl+F and by mouse.
    let mut app = test_app(&[1, 1]);
    app.search_focus = false;
    app.on_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL)).expect("ctrl+f");
    assert!(app.search_focus, "Ctrl+F focuses the search row");
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
fn replay_button_renders_emoji_next_to_process() {
    // Kata: a `replay` emoji next to the process name — but only while a shell
    // is the foreground process.
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].process = "bash".into();
    app.tabs[0].shells[0].last_cmd = Some("deno task dev".into());
    let buf = render(&mut app, W, H);
    let row = row_text(&buf, 4, W);
    assert!(row.contains(&format!("└ bash  {REPLAY_LABEL}")), "button next to process: {row:?}");
    let byte = row.find(REPLAY_LABEL).expect("button");
    let x = row[..byte].chars().count() as u16; // column, not byte offset (multibyte gutter)
    let style = cell(&buf, x, 4).style();
    assert_eq!(style.fg, Some(REPLAY_COLOR), "button carries the replay color");
    assert!(style.add_modifier.contains(Modifier::BOLD));
    // A non-shell program owns the tty → no button, even with a captured command.
    app.tabs[0].shells[0].process = "deno".into();
    let buf = render(&mut app, W, H);
    assert!(!row_text(&buf, 4, W).contains(REPLAY_LABEL), "hidden while a program runs");
    // No last command → no button.
    app.tabs[0].shells[0].process = "bash".into();
    app.tabs[0].shells[0].last_cmd = None;
    let buf = render(&mut app, W, H);
    assert!(!row_text(&buf, 4, W).contains(REPLAY_LABEL), "hidden without a last command");
}

#[test]
fn replay_suppressed_while_non_shell_in_foreground() {
    // Kata app.md §94: replay (button hit-test + Alt+r) is offered only while a
    // shell owns the tty, never next to another program.
    let mut app = test_app(&[1]);
    app.tabs[0].shells[0].last_cmd = Some("echo hi".into());
    app.tabs[0].shells[0].process = "deno".into();
    let col = (6 + "deno".len() + 2) as u16;
    assert_eq!(app.replay_at(4, col), None, "no button while a program runs");
    // A shell in the foreground restores it.
    app.tabs[0].shells[0].process = "bash".into();
    let col = (6 + "bash".len() + 2) as u16;
    assert_eq!(app.replay_at(4, col), Some((0, 0)), "button back at the shell prompt");
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
        shell.writer.write_all(&[*b]).expect("write");
    }
    shell.writer.flush().expect("flush");
    assert!(wait_for(|| screen_contents(&shell).contains(": AB"), Duration::from_secs(10)), "typed");
    for b in [0x7f, b'C'] {
        shell.note_input(&[b]);
        shell.writer.write_all(&[b]).expect("write");
    }
    shell.writer.flush().expect("flush");
    assert!(wait_for(|| screen_contents(&shell).contains(": AC"), Duration::from_secs(10)), "edited");
    shell.note_input(b"\r");
    shell.writer.write_all(b"\r").expect("write");
    shell.writer.flush().expect("flush");
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
        shell.writer.write_all(&[*b]).expect("write");
    }
    shell.writer.flush().expect("flush");
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
    // Click on the button: the hit-test targets exactly the label span.
    app.tabs[0].shells[0].process = "bash".into();
    app.tabs[0].shells[0].last_cmd = Some("echo RICON_REPLAY_$((2+2))".into());
    let col = (6 + "bash".len() + 2) as u16; // `{bar}   └ bash` + two-space gap
    assert_eq!(app.replay_at(4, col), Some((0, 0)), "click lands on the button");
    assert_eq!(app.replay_at(4, col - 1), None, "gap before the button misses");
    assert_eq!(app.replay_at(3, col), None, "path row has no button");
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row: 4,
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
