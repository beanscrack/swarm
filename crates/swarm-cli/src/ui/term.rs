//! The terminal shell: raw mode, the event loop, and the [`UiHandle`] the rest
//! of the program talks through.
//!
//! One task owns the keyboard and the screen. Engine tasks never print: they
//! send [`Msg`]s. That single-writer rule is what the old UI lacked.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size, supports_keyboard_enhancement};
use futures::{FutureExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::app::{Action, App, Effect, Info, Msg, Options, Status};
use super::dialog::DialogSpec;
use super::history::History;
use super::screen::LiveScreen;
use crate::style::Color;

/// Set while the terminal is in the state we put it in, so the panic hook and
/// `Drop` both know whether there is anything to undo.
static TERMINAL_CLAIMED: AtomicBool = AtomicBool::new(false);
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
/// Whether the terminal answered the keyboard-protocol query at startup. Asked
/// once, before the event stream exists: asking again while it reads would race.
static KEYBOARD_SUPPORTED: AtomicBool = AtomicBool::new(false);

/// Put the terminal back the way we found it. Safe to call twice and from a
/// panic hook; it only does something the first time.
fn restore_terminal() {
    if !TERMINAL_CLAIMED.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste, Show);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// Owns raw mode for as long as it lives.
struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        TERMINAL_CLAIMED.store(true, Ordering::SeqCst);
        let mut out = io::stdout();
        execute!(out, EnableBracketedPaste)?;
        // Kitty-protocol terminals can tell Shift+Enter from Enter. Asking is
        // cheap on terminals that answer; `SWARM_KEYBOARD=legacy` skips it.
        let legacy = std::env::var("SWARM_KEYBOARD").is_ok_and(|value| value == "legacy");
        if !legacy && supports_keyboard_enhancement().unwrap_or(false) {
            KEYBOARD_SUPPORTED.store(true, Ordering::SeqCst);
        }
        push_keyboard_flags(&mut out);
        install_panic_hook();
        Ok(Self)
    }

    /// Hand the terminal back temporarily (suspend, `$EDITOR`).
    fn release(&self) {
        restore_terminal();
    }

    fn reclaim(&self) -> io::Result<()> {
        enable_raw_mode()?;
        TERMINAL_CLAIMED.store(true, Ordering::SeqCst);
        let mut out = io::stdout();
        execute!(out, EnableBracketedPaste)?;
        push_keyboard_flags(&mut out);
        Ok(())
    }
}

fn push_keyboard_flags(out: &mut io::Stdout) {
    if KEYBOARD_SUPPORTED.load(Ordering::SeqCst)
        && execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)).is_ok()
    {
        KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // A panic in raw mode would leave the shell without echo.
            restore_terminal();
            previous(info);
        }));
    });
}

/// Without a terminal there is no UI task to queue what is typed during a turn,
/// so the plain [`Ui`] holds input back while the session is busy and delivers
/// it afterwards, which is what a person at the live prompt gets.
#[derive(Default)]
struct PlainGate {
    busy: AtomicBool,
    changed: tokio::sync::Notify,
}

/// How the rest of the program talks to the UI. Cheap to clone.
#[derive(Clone)]
pub struct UiHandle {
    tx: Option<mpsc::UnboundedSender<Msg>>,
    colors: Color,
    queued: Arc<AtomicUsize>,
    /// Tests read what was printed from here instead of stdout.
    captured: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// Present for handles with no terminal behind them.
    gate: Option<Arc<PlainGate>>,
}

impl UiHandle {
    /// A handle that writes straight to stdout (pipes, CI, tests).
    pub fn plain(colors: Color) -> Self {
        Self { tx: None, colors, queued: Arc::default(), captured: None, gate: Some(Arc::default()) }
    }

    /// A handle that records printed rows instead of writing them (tests).
    #[cfg(test)]
    pub fn capture(colors: Color) -> (Self, Arc<std::sync::Mutex<Vec<String>>>) {
        let rows = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handle = Self { captured: Some(Arc::clone(&rows)), ..Self::plain(colors) };
        (handle, rows)
    }

    /// Messages the user has queued behind the running turn.
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }

    pub fn clear_screen(&self) {
        self.send(Msg::ClearScreen);
    }

    /// Open a file in `$EDITOR` (the terminal is handed over and taken back).
    pub fn edit_file(&self, path: PathBuf) {
        self.send(Msg::EditFile(path));
    }

    pub fn colors(&self) -> Color {
        self.colors
    }

    /// True when a live terminal UI is behind this handle.
    pub fn is_live(&self) -> bool {
        self.tx.is_some()
    }

    fn send(&self, msg: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg);
        }
    }

    /// Commit rows to the transcript.
    pub fn print(&self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        if let Some(rows) = &self.captured {
            if let Ok(mut rows) = rows.lock() {
                rows.extend(lines);
            }
            return;
        }
        match &self.tx {
            Some(_) => self.send(Msg::Print(lines)),
            None => {
                let mut out = io::stdout().lock();
                for line in lines {
                    let _ = writeln!(out, "{line}");
                }
                let _ = out.flush();
            }
        }
    }

    pub fn info(&self, info: Info) {
        self.send(Msg::Info(info));
    }

    pub fn busy(&self, on: bool) {
        self.send(Msg::Busy(on));
        if let Some(gate) = &self.gate {
            gate.busy.store(on, Ordering::SeqCst);
            gate.changed.notify_waiters();
        }
    }

    pub fn status(&self, status: Status) {
        self.send(Msg::Status(status));
    }

    pub fn prefill(&self, text: impl Into<String>) {
        self.send(Msg::Prefill(text.into()));
    }

    /// Ask the user to pick. `None` means they dismissed it (or there is no
    /// terminal to ask on).
    pub async fn choose(&self, spec: DialogSpec) -> Option<usize> {
        let tx = self.tx.as_ref()?;
        let (reply, answer) = oneshot::channel();
        tx.send(Msg::Dialog(spec, reply)).ok()?;
        answer.await.ok().flatten()
    }
}

pub struct StartOptions {
    pub colors: Color,
    pub history_path: Option<PathBuf>,
    pub workspace: PathBuf,
    /// False for `swarm run`: status and dialogs only.
    pub input: bool,
}

/// The UI: a handle to talk to it, and the stream of things the user did.
pub struct Ui {
    handle: UiHandle,
    actions: mpsc::UnboundedReceiver<Action>,
    task: Option<JoinHandle<()>>,
}

impl Ui {
    /// Take over the terminal. Fails (leaving it untouched) when raw mode is
    /// unavailable; the caller should fall back to [`Ui::plain`].
    pub fn start(options: StartOptions) -> io::Result<Self> {
        let guard = TermGuard::enter()?;
        let (cols, rows) = size().unwrap_or((80, 24));
        let history = match options.history_path {
            Some(path) => History::load(path),
            None => History::memory(),
        };
        let queued = Arc::new(AtomicUsize::new(0));
        let mut app = App::new(Options {
            queue_len: Arc::clone(&queued),
            colors: options.colors,
            history,
            workspace: options.workspace,
            input: options.input,
        });
        app.set_size(cols as usize, rows as usize);
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(app, msg_rx, action_tx, guard));
        Ok(Self {
            handle: UiHandle { tx: Some(msg_tx), colors: options.colors, queued, captured: None, gate: None },
            actions: action_rx,
            task: Some(task),
        })
    }

    /// No terminal: a stdin line reader and plain stdout. Each non-empty line
    /// is a submission; end of input is a quit.
    pub fn plain(colors: Color) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let stdin = io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Action::Quit);
                        break;
                    }
                    Ok(_) => {
                        let text = line.trim_end_matches(['\n', '\r']).to_string();
                        if !text.trim().is_empty() && tx.send(Action::Submit(text)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self { handle: UiHandle::plain(colors), actions: rx, task: None }
    }

    pub fn handle(&self) -> UiHandle {
        self.handle.clone()
    }

    /// A UI with no terminal that replays `actions` then quits, printing into
    /// the returned buffer (tests).
    #[cfg(test)]
    pub fn scripted(colors: Color, actions: Vec<Action>) -> (Self, Arc<std::sync::Mutex<Vec<String>>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        for action in actions {
            let _ = tx.send(action);
        }
        let _ = tx.send(Action::Quit);
        let (handle, rows) = UiHandle::capture(colors);
        (Self { handle, actions: rx, task: None }, rows)
    }

    /// The next thing the user did; `None` once the UI is gone.
    pub async fn recv(&mut self) -> Option<Action> {
        if let Some(gate) = self.handle.gate.clone() {
            // No terminal: input waits until the turn in flight is over.
            loop {
                let changed = gate.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !gate.busy.load(Ordering::SeqCst) {
                    break;
                }
                changed.await;
            }
        }
        self.actions.recv().await
    }

    /// Give the terminal back: the live region is erased and the cursor left on
    /// a clean row. Anything printed afterwards is ordinary output.
    pub async fn shutdown(mut self) {
        self.handle.send(Msg::Shutdown);
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
        }
        restore_terminal();
    }
}

async fn run(
    mut app: App,
    mut msgs: mpsc::UnboundedReceiver<Msg>,
    actions: mpsc::UnboundedSender<Action>,
    guard: TermGuard,
) {
    let (cols, rows) = size().unwrap_or((80, 24));
    let mut screen = LiveScreen::new(io::stdout(), cols as usize, rows as usize);
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut signals = Signals::new();

    let frame = app.view(screen.cols(), screen.rows(), Instant::now());
    let _ = screen.draw(&frame);

    'ui: loop {
        let mut out = Pending::default();
        let now = Instant::now();
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => {
                    out.event(&mut app, event, now);
                    // Drain whatever else is ready so a fast burst of keys (a
                    // paste into a terminal without bracketed paste) is one repaint.
                    while let Some(Some(Ok(event))) = events.next().now_or_never() {
                        out.event(&mut app, event, now);
                    }
                }
                // The keyboard went away (stdin closed): nothing more will arrive.
                Some(Err(_)) | None => {
                    let _ = actions.send(Action::Quit);
                    break 'ui;
                }
            },
            msg = msgs.recv() => match msg {
                Some(msg) => {
                    out.effects(app.on_msg(msg, now));
                    while let Ok(msg) = msgs.try_recv() {
                        out.effects(app.on_msg(msg, now));
                    }
                }
                None => break 'ui,
            },
            _ = tick.tick() => {
                // The frame is rebuilt below either way; `on_tick` expires timed
                // state, and an unchanged frame writes nothing.
                app.on_tick(now);
            },
            signal = signals.next() => match signal {
                Signal::Terminate => {
                    let _ = actions.send(Action::Quit);
                    break 'ui;
                }
                Signal::Continued => {
                    // Back from `fg`: the screen may have been scribbled on.
                    let _ = guard.reclaim();
                    let (cols, rows) = size().unwrap_or((80, 24));
                    app.set_size(cols as usize, rows as usize);
                    let frame = app.view(cols as usize, rows as usize, now);
                    let _ = screen.resize(cols as usize, rows as usize, &frame);
                    continue 'ui;
                }
            },
        }

        for action in out.actions.drain(..) {
            if actions.send(action).is_err() {
                break 'ui;
            }
        }
        if out.suspend {
            suspend(&guard, &mut screen);
            // `fg` brings us back through SIGCONT, handled above; if the
            // signal is not delivered, repaint here regardless.
            let (cols, rows) = size().unwrap_or((80, 24));
            app.set_size(cols as usize, rows as usize);
            let frame = app.view(cols as usize, rows as usize, Instant::now());
            let _ = screen.resize(cols as usize, rows as usize, &frame);
        }
        if let Some(text) = out.edit.take() {
            if let Some(edited) = edit_externally(&guard, &mut screen, &text) {
                out.effects(app.on_msg(Msg::Prefill(edited), now));
            }
            let (cols, rows) = size().unwrap_or((80, 24));
            app.set_size(cols as usize, rows as usize);
            let frame = app.view(cols as usize, rows as usize, Instant::now());
            let _ = screen.resize(cols as usize, rows as usize, &frame);
        }
        out.print = fit_rows(std::mem::take(&mut out.print), screen.cols());
        if let Some(path) = out.edit_file.take() {
            open_in_editor(&guard, &mut screen, &path);
            let (cols, rows) = size().unwrap_or((80, 24));
            app.set_size(cols as usize, rows as usize);
            let frame = app.view(cols as usize, rows as usize, Instant::now());
            let _ = screen.resize(cols as usize, rows as usize, &frame);
        }
        if let Some((cols, rows)) = out.resize {
            app.set_size(cols as usize, rows as usize);
            let frame = app.view(cols as usize, rows as usize, Instant::now());
            let _ = screen.resize(cols as usize, rows as usize, &frame);
            if !out.print.is_empty() {
                let frame = app.view(screen.cols(), screen.rows(), Instant::now());
                let _ = screen.print(&out.print, &frame);
            }
        } else {
            let frame = app.view(screen.cols(), screen.rows(), Instant::now());
            if out.clear {
                let _ = screen.clear_screen(&frame);
            }
            if out.print.is_empty() {
                let _ = screen.draw(&frame);
            } else {
                let _ = screen.print(&out.print, &frame);
            }
        }
        if out.quit {
            break 'ui;
        }
    }
    let _ = screen.finish();
    drop(guard);
}

/// Committed rows are never re-measured by the terminal for us, so a row wider
/// than the screen would wrap with no indent. Wrap it ourselves, hanging under
/// its own indentation.
fn fit_rows(rows: Vec<String>, cols: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if super::text::width(&row) <= cols {
            out.push(row);
            continue;
        }
        let plain = super::text::strip_ansi(&row);
        let indent = plain.chars().take_while(|ch| *ch == ' ').count();
        let hang = " ".repeat((indent + 2).min(cols / 2));
        out.extend(super::text::wrap_hang(&row, cols, "", &hang));
    }
    out
}

/// What one pass of the loop collected.
#[derive(Default)]
struct Pending {
    actions: Vec<Action>,
    print: Vec<String>,
    clear: bool,
    suspend: bool,
    edit: Option<String>,
    edit_file: Option<PathBuf>,
    quit: bool,
    resize: Option<(u16, u16)>,
}

impl Pending {
    fn event(&mut self, app: &mut App, event: Event, now: Instant) {
        match event {
            Event::Key(key) => self.effects(app.on_key(key, now)),
            Event::Paste(text) => app.on_paste(&text),
            Event::Resize(cols, rows) => self.resize = Some((cols, rows)),
            _ => {}
        }
    }

    fn effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Action(action) => self.actions.push(action),
                Effect::Print(lines) => self.print.extend(lines),
                Effect::ClearScreen => self.clear = true,
                Effect::Suspend => self.suspend = true,
                Effect::EditExternal(text) => self.edit = Some(text),
                Effect::EditFile(path) => self.edit_file = Some(path),
                Effect::Quit => self.quit = true,
            }
        }
    }
}

/// Ctrl+Z: give the terminal back, stop, and take it again on resume.
#[cfg(unix)]
fn suspend(guard: &TermGuard, screen: &mut LiveScreen<io::Stdout>) {
    let _ = screen.finish();
    guard.release();
    println!("[swarm suspended: run `fg` to resume]");
    // SAFETY: raising SIGTSTP on ourselves is the standard way to stop; it
    // returns after the shell continues us.
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
    let _ = guard.reclaim();
}

#[cfg(not(unix))]
fn suspend(_guard: &TermGuard, _screen: &mut LiveScreen<io::Stdout>) {}

/// `$VISUAL`, then `$EDITOR`, then `vi`.
fn editor_command() -> String {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|value| !value.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_string())
}

/// Run the editor on `path` with the terminal handed over, then take it back.
fn open_in_editor(guard: &TermGuard, screen: &mut LiveScreen<io::Stdout>, path: &std::path::Path) -> bool {
    let editor = editor_command();
    let mut parts = editor.split_whitespace();
    let Some(program) = parts.next() else { return false };
    let _ = screen.finish();
    guard.release();
    let status = std::process::Command::new(program).args(parts).arg(path).status();
    let _ = guard.reclaim();
    status.is_ok_and(|status| status.success())
}

/// Ctrl+G: edit the prompt in `$VISUAL` / `$EDITOR`.
fn edit_externally(guard: &TermGuard, screen: &mut LiveScreen<io::Stdout>, text: &str) -> Option<String> {
    let dir = std::env::temp_dir().join(format!("swarm-edit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("prompt.md");
    std::fs::write(&path, text).ok()?;
    let edited = if open_in_editor(guard, screen, &path) {
        std::fs::read_to_string(&path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&dir);
    edited.map(|text| text.trim_end().to_string())
}

enum Signal {
    Terminate,
    Continued,
}

/// SIGTERM / SIGINT / SIGHUP end the session cleanly; SIGCONT repaints after `fg`.
struct Signals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hup: tokio::signal::unix::Signal,
    #[cfg(unix)]
    cont: tokio::signal::unix::Signal,
}

impl Signals {
    #[cfg(unix)]
    fn new() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        Self {
            term: signal(SignalKind::terminate()).expect("install SIGTERM handler"),
            // Raw mode turns Ctrl+C into a key, so a SIGINT is one sent from
            // outside; it must still leave the terminal usable.
            int: signal(SignalKind::interrupt()).expect("install SIGINT handler"),
            hup: signal(SignalKind::hangup()).expect("install SIGHUP handler"),
            cont: signal(SignalKind::from_raw(libc::SIGCONT)).expect("install SIGCONT handler"),
        }
    }

    #[cfg(unix)]
    async fn next(&mut self) -> Signal {
        tokio::select! {
            _ = self.term.recv() => Signal::Terminate,
            _ = self.int.recv() => Signal::Terminate,
            _ = self.hup.recv() => Signal::Terminate,
            _ = self.cont.recv() => Signal::Continued,
        }
    }

    #[cfg(not(unix))]
    fn new() -> Self {
        Self {}
    }

    #[cfg(not(unix))]
    async fn next(&mut self) -> Signal {
        std::future::pending().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text::{strip_ansi, width};

    /// Regression: with piped input the session read the *next* line (and the
    /// end-of-input) while a turn was still running, and quit mid-turn.
    #[tokio::test]
    async fn a_plain_ui_holds_input_back_while_a_turn_runs() {
        let (mut ui, _rows) = Ui::scripted(Color::Plain, vec![Action::Submit("next goal".into())]);
        let handle = ui.handle();
        handle.busy(true);
        let held = tokio::time::timeout(Duration::from_millis(150), ui.recv()).await;
        assert!(held.is_err(), "nothing is delivered while busy: {held:?}");
        handle.busy(false);
        assert_eq!(ui.recv().await, Some(Action::Submit("next goal".into())));
        assert_eq!(ui.recv().await, Some(Action::Quit), "end of input arrives last");
    }

    #[tokio::test]
    async fn busy_can_toggle_while_a_receiver_is_waiting() {
        let (mut ui, _rows) = Ui::scripted(Color::Plain, vec![]);
        let handle = ui.handle();
        handle.busy(true);
        let waiter = tokio::spawn(async move { ui.recv().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "still held");
        handle.busy(false);
        let got = tokio::time::timeout(Duration::from_secs(2), waiter).await.expect("released").unwrap();
        assert_eq!(got, Some(Action::Quit));
    }

    #[test]
    fn rows_that_fit_pass_through_untouched() {
        let rows = vec!["short".to_string(), "  \u{23bf}  also short".to_string()];
        assert_eq!(fit_rows(rows.clone(), 40), rows);
    }

    #[test]
    fn a_long_row_wraps_under_its_own_indent() {
        let row = format!("  \u{23bf}  Interrupted \u{00b7} run {} is in the store", "x".repeat(60));
        let fitted = fit_rows(vec![row], 50);
        assert!(fitted.len() >= 2, "{fitted:?}");
        assert!(fitted.iter().all(|row| width(row) <= 50), "{fitted:?}");
        assert!(fitted[1..].iter().all(|row| strip_ansi(row).starts_with("    ")), "hanging indent: {fitted:?}");
    }

    #[test]
    fn styled_rows_keep_their_colour_across_the_wrap() {
        let row = format!("\x1b[31m{}\x1b[0m", "word ".repeat(30));
        let fitted = fit_rows(vec![row], 40);
        assert!(fitted.len() > 1);
        assert!(fitted.iter().all(|row| row.starts_with("\x1b[31m") || row.starts_with(' ') && row.contains("\x1b[31m")), "{fitted:?}");
        assert!(fitted.iter().all(|row| !row.contains("\x1b[31m") || row.ends_with("\x1b[0m")));
    }
}
