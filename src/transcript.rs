//! Session transcripts (kata ai.md): the console text of every shell running a
//! supported AI client, appended to a dated file while it is produced — so a
//! power cut, a crash or a closed tab can't take the conversation with it and
//! the agent can be re-fed the context it lost.
//!
//! The text is taken from the shell's own vt100 screen, never from the raw PTY
//! stream: the emulator has already resolved every repaint, cursor move and
//! spinner into final lines, so the file reads like the console did instead of
//! like the escape sequences that drew it. Two things are committed:
//!
//! * lines that **scrolled off** the screen — the terminal's own history, in
//!   final form (Claude Code and openclaude print into it, so their transcript
//!   is exact);
//! * a **screen snapshot** while an inner app holds the alternate screen
//!   (opencode's TUI scrolls its chat inside it, where nothing ever reaches the
//!   scrollback), taken only when the screen changed and no more often than
//!   `SNAPSHOT_EVERY`.
//!
//! What is still on screen has not been committed yet, and that is exactly the
//! newest — most valuable — context. It is mirrored to a `.tail` sidecar every
//! `TAIL_EVERY`, folded into the transcript by `close` and removed there; a
//! transcript left with a `.tail` beside it is one whose shell never got to
//! close it, and the two together are the whole session.
//!
//! One transcript per agent session: `close` runs when the agent leaves the
//! shell as well as when the shell itself goes, and the next agent detected in
//! the same shell opens a file of its own.

use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError, TryLockError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tui_term::vt100;

use crate::{expand_home, folder_name, stale};

/// Directory to write transcripts into; `off`/`0`/empty turns the feature off.
const DIR_VAR: &str = "RICON_TRANSCRIPTS";
/// Alternate-screen snapshots: at most one this often, and only when the screen
/// actually changed — an idle TUI writes nothing at all.
const SNAPSHOT_EVERY: Duration = Duration::from_secs(2);
/// A frame is snapshotted once the PTY has been silent this long: the repaint
/// is then finished, and the screen is the one the user is reading. A client
/// draws one frame in several writes, and the half-drawn screen between them —
/// which looks the same at the start of every repaint — is not it.
const SNAPSHOT_SETTLE: Duration = Duration::from_millis(400);
/// A client that never stops redrawing (a streaming answer) never settles, and
/// its chat scrolls away inside the alternate screen while it does. After this
/// long *without a pause* the screen is taken as it stands rather than lost.
const SNAPSHOT_FORCE: Duration = Duration::from_secs(10);
/// How often the not-yet-committed screen is mirrored to the `.tail` sidecar.
const TAIL_EVERY: Duration = Duration::from_secs(2);
/// A dated marker separates blocks written this far apart, so a transcript read
/// days later still says when each stretch happened.
const GAP: Duration = Duration::from_secs(60);
/// Scrollback committed when logging starts: the agent is detected a beat after
/// it starts printing, and that beat — plus the command that launched it — is
/// context worth keeping. Older history is the shell's, not the session's.
const CATCH_UP: usize = 500;
/// Committed history rows remembered to find the uncommitted ones by, once the
/// scrollback is full (see `Capture::uncommitted`).
const ANCHOR: usize = 16;
/// Longest client or folder name carried into a file name — `NAME_MAX` is 255
/// bytes, and a transcript that cannot be created is a transcript lost.
const NAME_PART: usize = 64;

/// What a transcript is opened for: the client, the model it was running, and
/// the shell it ran in. Resolved by the UI thread at detection time; the file
/// it names is created by the shell's own threads on the first write.
#[derive(Clone)]
pub struct Meta {
    pub agent: String,
    pub model: String,
    pub cwd: PathBuf,
    pub pid: u32,
}

/// One shell's transcript, shared between the UI thread (which arms it), that
/// shell's reader thread (which writes it) and a flush thread per session.
/// `armed` is read once per PTY chunk, so the common case — a shell with no
/// agent in it — costs a single load and never touches the mutex.
///
/// Lock order: `inner`, then the parser. The parser lock is held only while
/// the screen is read, never across a write, so a slow disk can stall this
/// shell's own output but never a frame.
pub struct Transcript {
    armed: AtomicBool,
    /// The parser's scrollback limit.
    cap: usize,
    /// Bumped by every `arm`: a flush thread serves only the session it was
    /// started for, and retires when that one ends.
    session: AtomicU64,
    /// Lines the reader thread's chunks pushed into the scrollback since the
    /// last capture pass, counted exactly by `process`; `UNCOUNTED` once any
    /// went uncounted.
    pushed: AtomicUsize,
    inner: Mutex<Inner>,
}

/// `Transcript::pushed` when the count is not known.
const UNCOUNTED: usize = usize::MAX;

struct Inner {
    capture: Capture,
    /// The session being transcribed; `None` while there is none. Left set
    /// (with `armed` down) when the file could not be opened, so the probe
    /// re-reporting the same agent does not retry — and spawn a flush thread —
    /// every second for the rest of the session.
    meta: Option<Meta>,
    sink: Option<Sink>,
}

/// The open file, the sidecar beside it, and its write clock.
struct Sink {
    file: File,
    tail: PathBuf,
    wrote: Instant,
    /// Appended since the last fsync.
    dirty: bool,
}

/// What a capture pass runs for: output that just landed (the shell's reader
/// thread), or the session's once-a-second flush. Output means the console is
/// being drawn on; the flush is what writes the sidecar and syncs the file —
/// an agent waiting for input emits nothing at all, and its answer is exactly
/// the context that must already be on disk when the power goes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pump {
    Output,
    Flush,
}

/// One capture pass, read off the screen under the parser lock.
#[derive(Default)]
struct Take {
    /// History lines to commit, in order.
    history: Vec<String>,
    /// A screen snapshot to commit, with a dated marker.
    snapshot: Vec<String>,
    /// The current screen and its digest, when the sidecar is due a refresh.
    tail: Option<(Vec<String>, u64)>,
}

/// The reading side of a transcript: what of the scrollback is committed, when
/// the last snapshot and mirror went out, and how the output is flowing.
struct Capture {
    /// The parser's scrollback limit: below it the buffer only grows, so its
    /// length counts new lines exactly; at it, old lines drop out as new ones
    /// arrive and the length stands still.
    cap: usize,
    /// Scrollback length after the last pass.
    logged: usize,
    /// Digests of the newest committed history rows, oldest first.
    anchor: VecDeque<u64>,
    /// The next pass is a session's first: it takes at most `CATCH_UP` lines.
    catch_up: bool,
    /// The start of a line the terminal soft-wrapped whose rest has not
    /// scrolled off yet: a line is committed whole, as it was printed, not cut
    /// at the terminal's width.
    pending: String,
    /// Last screen committed as a snapshot, and when.
    snap: u64,
    snapped: Instant,
    /// When output last arrived — the console is settled once this is old
    /// enough, which is the only reliable way to know a repaint has finished.
    drawn: Instant,
    /// When the current stretch of unbroken output began (or the last forced
    /// look at it was taken).
    burst: Instant,
    /// Last screen mirrored to the sidecar, and when.
    tail: u64,
    tailed: Instant,
}

impl Capture {
    fn new(cap: usize) -> Self {
        // The throttles start expired, so the first screen is recorded at once
        // rather than two seconds into the session.
        Self {
            cap,
            logged: 0,
            anchor: VecDeque::with_capacity(ANCHOR),
            catch_up: true,
            pending: String::new(),
            snap: 0,
            snapped: stale(SNAPSHOT_EVERY),
            drawn: stale(SNAPSHOT_SETTLE),
            burst: Instant::now(),
            tail: 0,
            tailed: stale(TAIL_EVERY),
        }
    }

    /// Everything `screen` is ready to hand over. Runs under the parser lock:
    /// it reads and restores the scrollback view, allocates only the lines it
    /// returns, and does no IO.
    fn take(&mut self, screen: &mut vt100::Screen, pump: Pump, pushed: Option<usize>) -> Take {
        if pump == Pump::Output {
            if self.drawn.elapsed() >= SNAPSHOT_SETTLE {
                self.burst = Instant::now();
            }
            self.drawn = Instant::now();
        }
        let mut take = Take::default();
        if screen.alternate_screen() {
            // A full-screen client's scrollback stays empty for as long as it
            // holds the alternate screen — the screen itself is the only record.
            take.snapshot = self.snapshot(screen);
        } else {
            take.history = self.scrolled_off(screen, pushed);
        }
        if pump == Pump::Flush {
            take.tail = self.mirror(screen);
        }
        take
    }

    /// Scrollback lines that appeared since the last pass, oldest first.
    ///
    /// The view is moved to read them and put back before the lock is released,
    /// so no frame — and no live selection — can see it shift. Only what has
    /// left the screen is taken: it is final, whereas the screen is still being
    /// drawn on (that is what the sidecar is for).
    fn scrolled_off(&mut self, screen: &mut vt100::Screen, pushed: Option<usize>) -> Vec<String> {
        let (rows, cols) = screen.size();
        if rows == 0 {
            return Vec::new();
        }
        let view = screen.scrollback();
        let total = history(screen);
        let (uncommitted, adjoins) = self.uncommitted(screen, total, pushed);
        let fresh = if std::mem::replace(&mut self.catch_up, false) {
            uncommitted.min(CATCH_UP)
        } else {
            uncommitted
        };
        // The anchor must be the rows right before the ones taken now; after a
        // gap (history skipped by a catch-up, or the anchor lost) it restarts,
        // and so does a wrapped line begun before the gap.
        if !adjoins || fresh < uncommitted {
            self.anchor.clear();
            self.pending.clear();
        }
        let mut lines = Vec::with_capacity(fresh);
        let mut next = total - fresh;
        while next < total {
            // The view at offset `total - next` starts with row `next`.
            screen.set_scrollback(total - next);
            let n = (total - next).min(usize::from(rows));
            let texts: Vec<String> = screen.rows(0, cols).take(n).collect();
            for (at, text) in texts.into_iter().enumerate() {
                if self.anchor.len() == ANCHOR {
                    self.anchor.pop_front();
                }
                self.anchor.push_back(digest(&text));
                self.pending.push_str(&text);
                if !screen.row_wrapped(at as u16) {
                    lines.push(std::mem::take(&mut self.pending));
                }
            }
            next += n;
        }
        screen.set_scrollback(view);
        self.logged = total;
        lines
    }

    /// How many of the `total` scrollback rows — counted from the newest — are
    /// not committed yet, and whether they directly follow the anchor.
    ///
    /// Normally the reader thread has counted them as they were pushed
    /// (`Transcript::process`), and the newest committed row sitting right
    /// above them confirms it. Without a count (the session's first pass, a
    /// chunk that entered the alternate screen) the buffer's growth is the
    /// answer until it is full, confirmed the same way. A full buffer drops a
    /// line for every one it takes, so its length says nothing: the committed
    /// rows are then found by content — the smallest shift at which the newest
    /// `ANCHOR` rows committed reappear, which only a run of exactly those rows
    /// printed again in an uncounted chunk could fool. A shell reset (the
    /// buffer cleared) or a flood past the whole buffer leaves no anchor to
    /// find, and every row is new.
    fn uncommitted(&self, screen: &mut vt100::Screen, total: usize, pushed: Option<usize>) -> (usize, bool) {
        let Some(&newest) = self.anchor.back() else {
            // Below the cap the growth is exact whatever processed the bytes.
            let grown = total.saturating_sub(self.logged);
            let count = if total < self.cap { grown } else { pushed.unwrap_or(grown) };
            return (count.min(total), false);
        };
        if let Some(count) = pushed.filter(|&count| count < total)
            && row_digest(screen, total, total - 1 - count) == Some(newest)
        {
            return (count, true);
        }
        if total < self.cap
            && (1..=total).contains(&self.logged)
            && row_digest(screen, total, self.logged - 1) == Some(newest)
        {
            return (total - self.logged, true);
        }
        let mut anchored = |shift: usize| {
            self.anchor.iter().rev().enumerate().all(|(back, &want)| {
                (total - 1 - shift)
                    .checked_sub(back)
                    .is_some_and(|row| row_digest(screen, total, row) == Some(want))
            })
        };
        match (0..total.saturating_sub(self.anchor.len() - 1)).find(|&shift| anchored(shift)) {
            Some(shift) => (shift, true),
            None => (total, false),
        }
    }

    /// The screen, once the repaint that drew it has finished and it says
    /// something the last snapshot did not.
    ///
    /// Waiting for the frame to settle is what makes the snapshots readable: a
    /// client repaints in several writes (clear, then header, then body), so a
    /// check that fires between them sees a torn — often nearly empty — screen,
    /// and committing that both writes rubbish and, through the dedup below,
    /// hides the finished frame that follows it.
    fn snapshot(&mut self, screen: &mut vt100::Screen) -> Vec<String> {
        let settled = self.drawn.elapsed() >= SNAPSHOT_SETTLE;
        // Redrawn without a pause for so long that holding out for a still
        // frame would lose more than a torn one costs.
        let forced = !settled && self.burst.elapsed() >= SNAPSHOT_FORCE;
        if !(settled || forced) || self.snapped.elapsed() < SNAPSHOT_EVERY {
            return Vec::new();
        }
        if forced {
            // One forced look per `SNAPSHOT_FORCE` of unbroken output, not one
            // per chunk once the first is due.
            self.burst = Instant::now();
        }
        let lines = compact(visible(screen));
        let hash = digest(&lines);
        // Nothing new since the last snapshot: the client is repainting, not
        // saying anything. `snapped` stays where it is, so the next settled
        // frame is taken the moment it differs rather than two seconds later.
        if hash == self.snap {
            return Vec::new();
        }
        self.snap = hash;
        self.snapped = Instant::now();
        lines
    }

    /// The screen for the `.tail` sidecar, when it is due and has changed.
    /// The digest is recorded only once the sidecar is written (`Inner::write`),
    /// so a failed write is retried rather than taken for done.
    fn mirror(&mut self, screen: &mut vt100::Screen) -> Option<(Vec<String>, u64)> {
        if self.tailed.elapsed() < TAIL_EVERY {
            return None;
        }
        self.tailed = Instant::now();
        let lines = compact(visible(screen));
        let hash = digest(&lines);
        (hash != self.tail).then_some((lines, hash))
    }
}

impl Transcript {
    /// A transcript for a shell whose parser keeps `scrollback` lines.
    pub fn new(scrollback: usize) -> Self {
        Self {
            armed: AtomicBool::new(false),
            cap: scrollback,
            session: AtomicU64::new(0),
            pushed: AtomicUsize::new(UNCOUNTED),
            inner: Mutex::new(Inner { capture: Capture::new(scrollback), meta: None, sink: None }),
        }
    }

    /// Read whatever `parser` is ready to hand over and write it out.
    ///
    /// Called with `Pump::Output` from the shell's reader thread after every
    /// chunk it parses, and with `Pump::Flush` once a second from the
    /// session's flush thread.
    pub fn pump(&self, parser: &Mutex<vt100::Parser>, pump: Pump) {
        if !self.armed() {
            return;
        }
        let mut inner = self.lock();
        if inner.meta.is_none() {
            return; // closed while this pass waited for the lock
        }
        let take = {
            let mut parser = parser.lock().unwrap_or_else(PoisonError::into_inner);
            inner.capture.take(parser.screen_mut(), pump, self.take_pushed())
        };
        if inner.write(take, pump).is_none() {
            // Nowhere to write (no HOME, unwritable directory): stop trying
            // rather than retrying per chunk for the rest of the session.
            self.armed.store(false, Ordering::Release);
        }
    }

    /// Start transcribing a session, returning its number when this call
    /// started one — the caller then runs its flush thread. `None` when a
    /// session is already on, when transcripts are off, or when a close is
    /// still writing: this runs on the UI thread after every probe, so it never
    /// waits for the lock the writers hold; the next probe simply asks again.
    /// IO-free: the file is created by the first write.
    pub fn arm(&self, meta: Meta) -> Option<u64> {
        if self.armed() || transcript_dir().is_none() {
            return None;
        }
        let mut inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        if inner.meta.is_some() {
            return None;
        }
        inner.meta = Some(meta);
        inner.capture.catch_up = true;
        // Whatever was pushed before the session is history the catch-up
        // takes by the buffer, not by a count.
        self.pushed.store(UNCOUNTED, Ordering::Release);
        let session = self.session.fetch_add(1, Ordering::AcqRel) + 1;
        self.armed.store(true, Ordering::Release);
        Some(session)
    }

    /// Is this shell being transcribed? False before an agent is detected in
    /// it and again once its transcript is closed.
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }

    /// Parse `bytes` into `parser`'s screen — what the reader thread does with
    /// every chunk — counting, while a session is on, the lines it pushes into
    /// the scrollback. The emulator counts them itself for a view scrolled
    /// back: it raises the offset one row per pushed line to keep that view on
    /// its text. So the chunk is parsed with the view scrolled back by at least
    /// one row, and the rise is the count; the view is then left where the
    /// emulator would have left the user's own. All under the one parser lock,
    /// so no frame ever sees the borrowed offset. No count when the chunk
    /// touched the alternate screen, when the offset hit the top of the buffer,
    /// or when the chunk reset it.
    pub fn process(&self, parser: &mut vt100::Parser, bytes: &[u8]) {
        if !self.armed() {
            parser.process(bytes);
            return;
        }
        let screen = parser.screen_mut();
        let (alternate, view) = (screen.alternate_screen(), screen.scrollback());
        let before = history(screen);
        let probe = view.max(1);
        screen.set_scrollback(probe);
        parser.process(bytes);
        let screen = parser.screen_mut();
        let risen = screen.scrollback();
        let after = history(screen);
        let count = if alternate || screen.alternate_screen() {
            None
        } else if before == 0 {
            // Nothing to scroll back into yet — and nothing evicted either.
            (after < self.cap).then_some(after)
        } else {
            (risen >= probe && risen < after).then(|| risen - probe)
        };
        screen.set_scrollback(if view == 0 { 0 } else { risen });
        let _ = self.pushed.fetch_update(Ordering::AcqRel, Ordering::Acquire, |pushed| {
            Some(match count {
                Some(count) if pushed != UNCOUNTED => pushed.saturating_add(count).min(UNCOUNTED - 1),
                _ => UNCOUNTED,
            })
        });
    }

    /// The pushed-line count since the last pass, reset for the next. Taken
    /// under the parser lock, so no chunk lands between the count and the
    /// screen it describes.
    fn take_pushed(&self) -> Option<usize> {
        Some(self.pushed.swap(0, Ordering::AcqRel)).filter(|&pushed| pushed != UNCOUNTED)
    }

    /// Is `session` the one still being transcribed? What a flush thread
    /// checks before each pass.
    pub fn serves(&self, session: u64) -> bool {
        self.armed() && self.session.load(Ordering::Acquire) == session
    }

    /// Close the session: commit what scrolled off since the last pass and the
    /// screen the scrollback never saw, sync, and only then drop the sidecar
    /// that was standing in for them. A no-op without a session, so it can run
    /// from every end — the agent leaving the shell, the reader thread at EOF,
    /// and the shell's own teardown *before* it kills the child (an app clearing
    /// the screen on the way out must not erase the last thing it said).
    pub fn close(&self, parser: &Mutex<vt100::Parser>) {
        self.armed.store(false, Ordering::Release);
        let mut inner = self.lock();
        if inner.meta.is_none() {
            return;
        }
        let Inner { capture, .. } = &mut *inner;
        let (history, screen) = {
            let mut parser = parser.lock().unwrap_or_else(PoisonError::into_inner);
            let screen = parser.screen_mut();
            let alternate = screen.alternate_screen();
            let history =
                if alternate { Vec::new() } else { capture.scrolled_off(screen, self.take_pushed()) };
            let mut last = visible(screen);
            // A wrapped line that began in the scrollback ends on screen.
            let begun = std::mem::take(&mut capture.pending);
            if !begun.is_empty() {
                match last.first_mut() {
                    Some(first) => first.insert_str(0, &begun),
                    None => last.push(begun),
                }
            }
            let last = compact(last);
            // The alternate screen's last frame may be the last snapshot.
            let fresh = !(alternate && digest(&last) == capture.snap);
            (history, if fresh { last } else { Vec::new() })
        };
        inner.finish(&history, &screen);
        inner.meta = None;
        inner.sink = None;
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    /// Write out one capture pass. `None` when the file cannot be opened.
    fn write(&mut self, take: Take, pump: Pump) -> Option<()> {
        if take.history.is_empty() && take.snapshot.is_empty() && take.tail.is_none() && self.sink.is_none() {
            return Some(()); // nothing to say yet: no empty file either
        }
        let sink = self.open()?;
        // Write errors (a full disk) are not fatal: the next pass tries again.
        let _ = sink.append(&take.history, false);
        let _ = sink.append(&take.snapshot, true);
        let mirrored = take.tail.and_then(|(lines, hash)| sink.mirror(&lines).is_ok().then_some(hash));
        if pump == Pump::Flush {
            let _ = sink.sync();
        }
        if let Some(hash) = mirrored {
            self.capture.tail = hash;
        }
        Some(())
    }

    /// Fold the last text in and seal the file. The sidecar is removed only
    /// once everything it held is synced into the transcript.
    fn finish(&mut self, history: &[String], screen: &[String]) {
        if self.sink.is_none() && history.is_empty() && screen.is_empty() {
            return; // never armed long enough to say anything: no empty file
        }
        let Some(sink) = self.open() else { return };
        let sealed =
            sink.append(history, false).and_then(|()| sink.append(screen, true)).and_then(|()| sink.sync());
        if sealed.is_ok() {
            let _ = std::fs::remove_file(&sink.tail);
        }
    }

    /// The open file, created on first use.
    fn open(&mut self) -> Option<&mut Sink> {
        if self.sink.is_none() {
            self.sink = Sink::create(self.meta.as_ref()?);
        }
        self.sink.as_mut()
    }
}

impl Sink {
    /// Create today's transcript for `meta`: a directory per date, and a name
    /// carrying the time, the client, the folder it ran in and the shell's pid
    /// — so two agents started in the same second in the same folder still get
    /// a file each.
    fn create(meta: &Meta) -> Option<Self> {
        let now = now();
        let dir = transcript_dir()?.join(now.date());
        std::fs::create_dir_all(&dir).ok()?;
        let name =
            format!("{}-{}-{}-{}", now.compact(), tame(&meta.agent), tame(&folder_name(&meta.cwd)), meta.pid);
        let path = dir.join(format!("{name}.txt"));
        let mut file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
        let header = format!(
            "# ricon session transcript\n# started {}\n# agent   {} ({})\n# cwd     {}\n\n",
            now.full(),
            meta.agent,
            meta.model,
            meta.cwd.display()
        );
        file.write_all(header.as_bytes()).ok()?;
        Some(Self { file, tail: dir.join(format!("{name}.tail.txt")), wrote: Instant::now(), dirty: true })
    }

    /// Append committed lines in one write. A dated marker goes in front of a
    /// snapshot — which is only legible with the time it was taken — and in
    /// front of the first block after a quiet stretch.
    fn append(&mut self, lines: &[String], snapshot: bool) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut text = String::new();
        if snapshot || self.wrote.elapsed() >= GAP {
            text.push_str(&format!("\n──── {} ────\n", now().time()));
        }
        for line in lines {
            text.push_str(line.trim_end());
            text.push('\n');
        }
        self.wrote = Instant::now();
        self.dirty = true;
        self.file.write_all(text.as_bytes())
    }

    /// Flush what was appended since the last sync to the disk. A transcript
    /// that only reached the page cache is precisely the transcript a power
    /// cut takes with it.
    fn sync(&mut self) -> io::Result<()> {
        if self.dirty {
            self.file.sync_data()?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Replace the sidecar with the screen as it stands — written, flushed to
    /// the disk and only then renamed into place, so the file a power cut
    /// leaves behind is always a whole screen and never half of one.
    fn mirror(&self, lines: &[String]) -> io::Result<()> {
        let mut text = lines.join("\n");
        text.push('\n');
        let staged = self.tail.with_extension("new");
        let written = File::create(&staged).and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_data()
        });
        match written.and_then(|()| std::fs::rename(&staged, &self.tail)) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = std::fs::remove_file(&staged);
                Err(error)
            }
        }
    }
}

/// The live screen as lines, trailing blanks dropped. Read at scrollback offset
/// 0 whatever the user is looking at, and the view is restored before the lock
/// is released so no frame can see it move.
fn visible(screen: &mut vt100::Screen) -> Vec<String> {
    let (_, cols) = screen.size();
    let view = screen.scrollback();
    screen.set_scrollback(0);
    let texts: Vec<String> = screen.rows(0, cols).collect();
    let mut lines = Vec::new();
    let mut line = String::new();
    for (row, text) in texts.into_iter().enumerate() {
        line.push_str(&text);
        // Soft-wrapped rows are one line, as they were printed.
        if !screen.row_wrapped(row as u16) {
            lines.push(std::mem::take(&mut line));
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    screen.set_scrollback(view);
    trimmed(lines)
}

/// How many lines `screen`'s scrollback holds. Leaves the view at the top.
fn history(screen: &mut vt100::Screen) -> usize {
    // `set_scrollback` clamps to the buffer, so asking for everything reports
    // its length.
    screen.set_scrollback(usize::MAX);
    screen.scrollback()
}

/// Digest of scrollback row `row` (0 = oldest) of a buffer `total` rows long.
/// Moves the view; the caller restores it.
fn row_digest(screen: &mut vt100::Screen, total: usize, row: usize) -> Option<u64> {
    let (_, cols) = screen.size();
    screen.set_scrollback(total - row);
    screen.rows(0, cols).next().map(|line| digest(&line))
}

/// `lines` without its trailing blank rows — the empty bottom of a screen is
/// padding, not content.
fn trimmed(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// A screen dump with its blank padding collapsed: the empty rows between a
/// client's panes are layout, and layout is not context. Only whole-screen
/// dumps are compacted — scrolled-off history is committed exactly as it was.
fn compact(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        let blank = line.trim().is_empty();
        if blank && out.last().is_some_and(|last: &String| last.trim().is_empty()) {
            continue;
        }
        out.push(if blank { String::new() } else { line });
    }
    out
}

fn digest<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// `s` reduced to characters that are safe in a file name, and short enough
/// to leave room for the rest of it.
fn tame(s: &str) -> String {
    let kept: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .take(NAME_PART)
        .collect();
    if kept.is_empty() { "shell".into() } else { kept }
}

/// Where transcripts are written: `$RICON_TRANSCRIPTS` when it names a
/// directory, nowhere when it is `off`/`0`/empty, and otherwise
/// `$XDG_DATA_HOME` (or `~/.local/share`) `/ricon/sessions`.
pub fn transcript_dir() -> Option<PathBuf> {
    match std::env::var(DIR_VAR) {
        Ok(value) if matches!(value.trim(), "" | "0" | "off" | "no" | "false") => None,
        Ok(value) => Some(expand_home(value.trim())),
        Err(_) => {
            let base = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")))?;
            Some(base.join("ricon").join("sessions"))
        }
    }
}

// ── wall clock ───────────────────────────────────────────────────────────────

/// A moment as the fields a person reads it in.
#[derive(Debug, PartialEq, Eq)]
pub struct Stamp {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

impl Stamp {
    /// `YYYY-MM-DD` — the transcript's directory, and what "dated file" means.
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `HH:MM:SS` for the markers inside a transcript.
    pub fn time(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.min, self.sec)
    }

    /// `HHMMSS` for the file name, where colons would be unwelcome.
    pub fn compact(&self) -> String {
        format!("{:02}{:02}{:02}", self.hour, self.min, self.sec)
    }

    pub fn full(&self) -> String {
        format!("{} {}", self.date(), self.time())
    }
}

/// Local wall clock, now.
pub fn now() -> Stamp {
    let unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let unix = i64::try_from(unix).unwrap_or(i64::MAX);
    stamp(unix.saturating_add(local_offset()))
}

/// Civil date and time of a Unix timestamp — Howard Hinnant's civil-from-days,
/// which is exact for every date, leap years and century rules included.
pub fn stamp(secs: i64) -> Stamp {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    // Every field below is range-bound by the arithmetic (month 1..=12, day
    // 1..=31, hour < 24, …), so the narrowing casts cannot truncate.
    Stamp {
        year: yoe + era * 400 + i64::from(month <= 2),
        month: month as u32,
        day: (doy - (153 * mp + 2) / 5 + 1) as u32,
        hour: (rest / 3_600) as u32,
        min: (rest % 3_600 / 60) as u32,
        sec: (rest % 60) as u32,
    }
}

/// Seconds between UTC and this machine's local time. The standard library
/// cannot read the zone database, but the SQLite already bundled in the tree
/// can — and a file dated in UTC would land on the wrong day for anyone working
/// either side of midnight. Re-read every minute: ricon runs for days, and a
/// daylight-saving change must not leave every marker an hour off.
fn local_offset() -> i64 {
    const FRESH: Duration = Duration::from_secs(60);
    static OFFSET: Mutex<Option<(Instant, i64)>> = Mutex::new(None);
    let mut cached = OFFSET.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((at, offset)) = *cached
        && at.elapsed() < FRESH
    {
        return offset;
    }
    let offset = rusqlite::Connection::open_in_memory()
        .and_then(|db| {
            db.query_row(
                "SELECT CAST(strftime('%s', datetime('now','localtime')) AS INTEGER) \
                      - CAST(strftime('%s', 'now') AS INTEGER)",
                [],
                |row| row.get(0),
            )
        })
        .unwrap_or(0);
    *cached = Some((Instant::now(), offset));
    offset
}
