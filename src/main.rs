//! Bayan (بيان) — an Arabic-first, agent-ready terminal.
//!
//! Milestone M1: one window, one PowerShell over ConPTY, correct Arabic
//! shaping and BiDi from day one. Architecture mirrors EasyTer (Python/Qt),
//! whose regression suites serve as the behavioral specification.

// release builds are pure GUI apps: no companion console window
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod a11y;
mod bidi;
mod bidi_ctl;
mod chrome;
mod cli;
mod config;
mod config_watch;
mod context_menu;
mod copyclean;
mod copymode;
mod elevate;
mod gpu;
mod groups;
mod handoff;
mod images;
mod input;
mod jumplist;
mod keybinds;
mod keyroute;
mod keys;
mod kitty;
mod lang;
mod latency;
mod layout;
mod link;
mod localpath;
mod lua_host;
mod mouse;
mod mru;
mod mux;
mod osc7;
mod persist;
mod plugins;
mod posh;
mod process;
mod pty_writer;
mod render;
mod screen_reader;
mod scroll;
mod scrollbar;
mod shellint;
mod shellmenu;
mod shells;
mod ssh;
mod term;
mod theme;
mod toast;

#[cfg(test)]
mod conpty_load;
#[cfg(test)]
#[path = "../vendor/portable-pty/src/win/conpty_resolve.rs"]
// Vendored portable-pty code: its unsafe blocks are upstream's to document.
#[allow(clippy::undocumented_unsafe_blocks)]
mod conpty_resolve;

/// rgb -> "#rrggbb" for the config file.
fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Meta copied off a pane before the term lock: command marks in grid
/// space, the running command, the BiDi snapshot, and evicted lines.
/// The `Arc` is the snapshot. Cloning it does not copy the stamp list.
type PaneSnap = (
    Vec<(usize, Option<i32>)>,
    String,
    std::sync::Arc<bidi_ctl::BidiState>,
    u64,
    Option<u64>,
    Option<bidi_ctl::BidiReorder>,
    bool,
);

/// The Windows caret rhythm; kitty's idea on top: stop after 15s idle.
const BLINK_MS: u128 = 530;
const BLINK_STOP: std::time::Duration = std::time::Duration::from_secs(15);

/// The shortcuts editor overlay's state.
#[derive(Default)]
struct ShortcutsState {
    sel: usize,
    /// waiting for the new chord's keypress
    capturing: bool,
    /// a one-shot footer message (conflict / invalid chord)
    flash: Option<String>,
}

/// What the close guard is protecting (confirm_close setting).
#[derive(Clone, Copy)]
enum CloseTarget {
    /// the focused pane of tab .0 (Ctrl+Shift+W)
    Pane(usize, usize),
    /// every pane in tab .0 (the tab-bar close control)
    Tab(usize),
    /// the whole window (the titlebar X)
    Window,
}

/// Return true when the window may close; otherwise retain a confirmation.
fn prepare_window_close(pending: &mut Option<CloseTarget>, confirm: bool, running: usize) -> bool {
    if matches!(pending, Some(CloseTarget::Window)) {
        return false;
    }
    if confirm && running > 0 {
        *pending = Some(CloseTarget::Window);
        return false;
    }
    *pending = None;
    true
}

fn consume_guard_release(pending: &mut bool, state: ElementState, button: MouseButton) -> bool {
    if *pending && button == MouseButton::Left {
        *pending = false;
        state == ElementState::Released
    } else {
        false
    }
}

/// The system notification sound (bell mode "sound"). Async — never blocks
/// the UI thread.
fn beep() {
    // SAFETY: MessageBeep takes one integer sound type and no pointers,
    // and is safe to call from any thread.
    unsafe {
        // windows-sys files MessageBeep under Diagnostics::Debug (user32's
        // winuser.h function, but that's where the metadata puts it)
        windows_sys::Win32::System::Diagnostics::Debug::MessageBeep(0);
    }
}

/// The quake hotkey (Ctrl+Alt+`) fires from ANYWHERE via RegisterHotKey;
/// winit's msg hook flags it and about_to_wait toggles the window.
static QUAKE_HIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// A toast/tray click wants the window raised (summon, never hide).
static SUMMON_HIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `WM_SETTINGCHANGE` / `WM_SYSCOLORCHANGE` arrived. `about_to_wait` re-reads
/// High Contrast and the text-size slider; the hook only sets the bit.
static A11Y_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const QUAKE_ID: usize = 0xB1AA;

/// The letter a Ctrl+<letter> event represents, resolved the way real
/// terminals do it: by the PHYSICAL key position first. This is what makes
/// Ctrl+C/Ctrl+F work on an Arabic keyboard layout, where the F key's
/// logical character is "ب" — and it also covers the Windows quirk of Ctrl
/// composing C0 control chars into the logical key (Ctrl+F = '\u{6}').
fn key_letter(event: &KeyEvent) -> Option<char> {
    if let winit::keyboard::PhysicalKey::Code(c) = event.physical_key {
        if let Some(l) = keys::pc101_unshifted(c).filter(char::is_ascii_lowercase) {
            return Some(l);
        }
    }
    ctrl_letter(&event.logical_key)
}

/// The preset `dir` steps away from the nearest one at or above `cur`
/// (the last preset when `cur` is past them all), clamped to the list.
fn step_preset<T: PartialOrd + Copy>(steps: &[T], cur: T, dir: i32) -> T {
    let i = steps
        .iter()
        .position(|&s| s >= cur)
        .unwrap_or(steps.len() - 1) as i32;
    steps[(i + dir).clamp(0, steps.len() as i32 - 1) as usize]
}

/// Fallback for events without a physical letter key (VK_PACKET injection,
/// exotic hardware): decode from the logical key, including Windows'
/// pre-composed control chars.
fn ctrl_letter(key: &Key) -> Option<char> {
    if let Key::Character(s) = key {
        let c = s.chars().next()?;
        if ('\u{1}'..='\u{1a}').contains(&c) {
            return Some((c as u8 + 96) as char); // \x06 -> 'f'
        }
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_lowercase());
        }
    }
    None
}

/// The literal-text search core, GUI-free so a real Term can exercise it in
/// tests: step past the current hit, search in `dir`, wrap from the far end.
fn find_match<T: alacritty_terminal::event::EventListener>(
    t: &mut alacritty_terminal::term::Term<T>,
    query: &str,
    hl: &Option<Match>,
    dir: Direction,
) -> Option<Match> {
    let Ok(mut rs) = RegexSearch::new(&term::regex_escape(query)) else {
        return None;
    };
    let cols = t.columns();
    let top = -(t.grid().history_size() as i32);
    let bottom = t.screen_lines() as i32 - 1;
    let origin = match hl {
        Some(m) => {
            let p = if matches!(dir, Direction::Right) {
                *m.end()
            } else {
                *m.start()
            };
            step_point(p, cols, top, bottom, dir)
        }
        None => Point::new(Line(bottom), Column(cols.saturating_sub(1))),
    };
    let side = if matches!(dir, Direction::Right) {
        Side::Left
    } else {
        Side::Right
    };
    t.search_next(&mut rs, origin, dir, side, None).or_else(|| {
        // wrap around from the far end
        let wrap = match dir {
            Direction::Left => Point::new(Line(bottom), Column(cols.saturating_sub(1))),
            Direction::Right => Point::new(Line(top), Column(0)),
        };
        t.search_next(&mut rs, wrap, dir, side, None)
    })
}

/// The display offset that brings the previous/next prompt to the top of
/// the view (EasyTer's _jump_command math). `abs_top` = absolute line of
/// the viewport's first row.
fn jump_offset(marks: &[usize], hist: usize, off: usize, dir: i32) -> Option<usize> {
    let abs_top = hist as i64 - off as i64;
    let target = if dir < 0 {
        marks
            .iter()
            .map(|&a| a as i64)
            .filter(|&a| a < abs_top)
            .max()
    } else {
        marks
            .iter()
            .map(|&a| a as i64)
            .filter(|&a| a > abs_top)
            .min()
    }?;
    Some((hist as i64 - target + 1).clamp(0, hist as i64) as usize)
}

/// Minimum gap between two toasts a PROGRAM asked for. Terminal output is
/// not trusted: a loop emitting OSC 777 (or `cat` of a file full of them)
/// would otherwise fire one balloon per read chunk and bury the
/// notification centre under text Bayan appears to have written itself.
const TOAST_MIN_GAP: std::time::Duration = std::time::Duration::from_secs(5);

/// May a PTY-triggered toast fire now? `last` is when one last did.
fn toast_due(last: Option<Instant>, now: Instant, gap: std::time::Duration) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= gap)
}

/// The active tab index after tab `removed` leaves a list of `len` tabs.
/// A background tab whose shell exits sits at a LOWER index than the active
/// one often enough (the first tab is the one people leave running): the
/// active index must slide down with it, or focus lands on the wrong tab.
/// The bool is whether the tab on screen is now a different one, which needs
/// the same resets as switching to it.
fn active_after_remove(active: usize, removed: usize, len_after: usize) -> (usize, bool) {
    if len_after == 0 {
        return (0, false);
    }
    let a = if removed < active { active - 1 } else { active };
    (a.min(len_after - 1), removed == active)
}

/// One cell step in the search direction, clamped to the buffer (used to
/// move the search origin past the current hit so Enter advances).
fn step_point(p: Point, cols: usize, top: i32, bottom: i32, dir: Direction) -> Point {
    match dir {
        Direction::Right => {
            if p.column.0 + 1 < cols {
                Point::new(p.line, Column(p.column.0 + 1))
            } else if p.line.0 < bottom {
                Point::new(Line(p.line.0 + 1), Column(0))
            } else {
                p
            }
        }
        Direction::Left => {
            if p.column.0 > 0 {
                Point::new(p.line, Column(p.column.0 - 1))
            } else if p.line.0 > top {
                Point::new(Line(p.line.0 - 1), Column(cols.saturating_sub(1)))
            } else {
                p
            }
        }
    }
}

/// Most tabs a restored session brings back (each one is a shell process).
const MAX_RESTORED_TABS: usize = 16;

/// Panes in one tab. `split` and session restore both stop here, so a
/// saved tab whose shells come back cannot grow past the live cap.
const MAX_PANES_PER_TAB: usize = 4;

/// A `BAYAN_*` debug hook from the environment (BAYAN-010). The hooks can
/// type into the shell (`BAYAN_TYPE`), drive the clipboard (`BAYAN_PASTE`)
/// and log to `%TEMP%` (`BAYAN_PROFILE`), so anything that could set
/// Bayan's environment — a shortcut, a parent process — would get those
/// powers. They exist for the build → launch → screenshot loop, so they are
/// compiled into debug builds and into a release build only on request:
/// `cargo build --release --features dev-hooks`. Everywhere else every hook
/// reads as unset.
#[cfg(any(debug_assertions, feature = "dev-hooks"))]
fn dev_hook(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

#[cfg(not(any(debug_assertions, feature = "dev-hooks")))]
fn dev_hook(_name: &str) -> Option<std::ffi::OsString> {
    None
}

/// Startup timeline profiling (EasyTer's `EASYTER_PROFILE` pattern): set
/// BAYAN_PROFILE=1 and marks land in %TEMP%\bayan_profile.log. Zero cost
/// when unset; a file because release builds have no console.
mod prof {
    use std::io::Write;
    use std::sync::OnceLock;
    use std::time::Instant;

    static T0: OnceLock<Instant> = OnceLock::new();

    pub fn init() {
        T0.get_or_init(Instant::now);
        if super::dev_hook("BAYAN_PROFILE").is_some() {
            let _ = std::fs::write(std::env::temp_dir().join("bayan_profile.log"), "");
        }
    }

    /// Whether marks are being recorded; lets a caller skip building one.
    /// Read once: the reader thread asks on every PTY read.
    pub fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| super::dev_hook("BAYAN_PROFILE").is_some())
    }

    static LATENCY: std::sync::Mutex<crate::latency::Latency> =
        std::sync::Mutex::new(crate::latency::Latency::new());

    fn latency() -> std::sync::MutexGuard<'static, crate::latency::Latency> {
        LATENCY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A key's bytes reached the focused pane; `arrived` is when winit
    /// delivered the key. Nothing about the key itself is recorded.
    pub fn key_sent(arrived: Instant) {
        if on() {
            latency().key_sent(arrived);
        }
    }

    /// A PTY read returned (any pane).
    pub fn output_read() {
        if on() {
            latency().output_read(Instant::now());
        }
    }

    /// A frame went to the GPU.
    pub fn frame_presented() {
        if !on() {
            return;
        }
        let mut l = latency();
        if let Some(s) = l.frame_presented(Instant::now()) {
            mark(&format!(
                "latency key->pty-output {:.1} ms, key->frame {:.1} ms",
                s.to_output_ms, s.to_frame_ms
            ));
            if let Some(summary) = l.summary() {
                mark(&summary);
            }
        }
    }

    pub fn mark(label: &str) {
        if !on() {
            return;
        }
        let t0 = *T0.get_or_init(Instant::now);
        let line = format!(
            "{:9.1} ms  {}\n",
            t0.elapsed().as_secs_f64() * 1000.0,
            label
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::temp_dir().join("bayan_profile.log"))
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

use lang::tr;
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::TermMode;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use term::{LockExt, Session, TermSize};

pub enum UserEvent {
    ShellProfiles(Vec<shells::Shell>),
    /// New PTY output was parsed in tab `id`: repaint (and busy-dot it).
    Wakeup(u64),
    /// Tab `id`'s emulator answers its child (DSR/DA replies TUIs block on).
    PtyWrite(u64, String),
    /// Input was rejected or the background pipe writer failed.
    PtyWriteFailed(u64, String),
    /// The font system finished loading on its background thread.
    RendererReady(Box<render::Renderer>),
    /// Pane `id` asked to set the clipboard via OSC 52. Applied only when
    /// `allow_osc52_write` says so — the default is to drop it, and a write
    /// from any pane but the focused one of the active tab is dropped too.
    ClipboardSet(u64, String),
    /// Pane `id` asked to read the clipboard (OSC 52). Forwarded so a later
    /// change can answer it. This build writes no reply. The read flag
    /// defaults off; see [`config::UserConfig::osc52_read_enabled`].
    ClipboardLoad(u64, alacritty_terminal::term::ClipboardType),
    /// Pane `id`'s shell reported its working directory (OSC 7 or OSC 9;9).
    Cwd(u64, String),
    /// Pane `id` rang the bell (Claude waiting for an approval).
    Bell(u64),
    /// A command that ran ≥6s in pane `id` just finished (ok?).
    CommandFinished(u64, term::CmdOutcome),
    /// Pane `id`'s program asked for a desktop notification (title, body).
    Notify(u64, String, String),
    /// Pane `id` reported OSC 9;4 progress (None clears it).
    Progress(u64, Option<term::Progress>),
    /// The prompt picker's worker rendered one preview: (generation, row,
    /// ANSI). Dropped when the picker that asked is no longer open.
    PoshPreview(u64, usize, Option<Vec<u8>>),
    /// Highlight rows a burst did not run: (generation, rows). The picker
    /// clears those claims so a later visit can ask again.
    PoshDropped(u64, Vec<usize>),
    /// Pane `id`'s shell exited.
    Exit(u64),
    /// A link open failed, or `settle` refused it, off the UI thread.
    /// `reason` is already [`link::RefuseReason::label`].
    LinkFailed {
        reason: String,
        shown: String,
    },
    /// Elevated `path:line` with no Program Files `Code.exe`. The card shows
    /// the full path; Enter reveals it in Explorer.
    LinkConfirm {
        path: String,
    },
    /// `config.json` changed on disk (or failed to parse). Never a panel save.
    ConfigReloaded(config_watch::ReloadMsg),
    /// AccessKit: a reader connected, asked for an action, or disconnected.
    /// The proxy path delivers these; the tree is built only on the first.
    AccessKit(accesskit_winit::Event),
}

/// A one-line notice for a config reload. A parse error stays until the file
/// is valid. A "takes effect later" line expires on its own. Neither blocks
/// the terminal.
#[derive(Clone)]
struct ConfigNotice {
    text: String,
    until: Option<Instant>,
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(event: accesskit_winit::Event) -> Self {
        UserEvent::AccessKit(event)
    }
}

/// The link under the pointer. Stable while the pointer stays inside it,
/// so a hover does not rebuild the frame on every pixel.
#[derive(PartialEq, Eq)]
struct LinkHover {
    pane_id: u64,
    /// Viewport row. `-1` is the history line a smooth scroll has
    /// brought into the top of the pane.
    row: i32,
    start_col: usize,
    end_col: usize,
    target: link::StatusParts,
}

/// A Ctrl+click that was refused, or a confirm card waiting for Enter.
/// Esc dismisses either. Enter runs `confirm` when it is set. A refusal
/// leaves it empty so Enter cannot launch the target. The variant is the
/// action: Enter does not guess from the text.
struct BlockedLink {
    reason: String,
    shown: String,
    confirm: Option<link::ConfirmOpen>,
    is_app_error: bool,
}

/// What a confirm card is holding, which decides where Enter sends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingKind {
    /// A clipboard paste (or a palette insert): goes where a paste goes,
    /// broadcast panes included.
    Paste,
    /// A dropped path, quoted for the focused pane's shell: that pane only
    /// (R-01) — another pane's shell would parse the quoting differently.
    Drop,
    /// A plugin action's command (BAYAN-007): the focused pane only,
    /// unbracketed, so a trailing CR still runs it.
    Command,
    /// config.lua or an executable plugin, approved by content hash.
    /// Enter does not type the source into the shell.
    ApproveLua,
}

/// Text held for the confirm card: the bytes to send, and why the card is up.
struct PendingPaste {
    kind: PendingKind,
    /// The pane the card was raised for (R-02). Enter sends nothing unless
    /// that pane is still the focused one: a card must never land in a
    /// different shell than the one it described.
    pane: Option<u64>,
    text: String,
    lines: usize,
    chars: usize,
    reason: String,
    /// Set when Enter approves a Lua file instead of typing `text`.
    lua: Option<lua_host::PendingScript>,
    /// Rows of a long approval card already scrolled off the top.
    scroll: usize,
    /// The last row of that card has been inside the window.
    saw_end: bool,
}

/// One terminal pane (a tab holds one or more).
struct Pane {
    id: u64,
    session: Session,
    /// the command line it was started with, so a restored session brings
    /// a Command Prompt tab back as Command Prompt
    command: String,
    /// when this pane last produced output — feeds the busy dot
    last_output: Option<Instant>,
    /// the program's OSC 9;4 progress report, until it clears it
    progress: Option<term::Progress>,
    /// Set when the ConPTY lives in the mux server. `armed` is cleared to
    /// detach: the session's kill hook then leaves the remote process running.
    mux: Option<MuxBind>,
}

struct MuxBind {
    id: u64,
    armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Orientation {
    Row,    // side by side (Ctrl+Shift+E)
    Column, // stacked (Ctrl+Shift+O)
}

/// One tab: a set of panes and the split tree that lays them out.
struct Tab {
    panes: Vec<Pane>,
    /// where each pane goes: side-by-side and stacked splits, nested in any
    /// mix (`layout.rs`); leaves are indices into `panes`
    layout: layout::Node,
    focused: usize,
    /// cwd basename of the focused pane (falls back to a plain name)
    title: String,
    /// a name the user gave the tab; wins over `title` until cleared
    name: Option<String>,
    /// a background pane rang the bell: show the amber dot until visited
    attention: bool,
    /// typed keys and pastes go to every pane of this tab, not just the
    /// focused one (Ctrl+Shift+B)
    broadcast: bool,
    /// the focused pane fills the tab; the others keep their PTY size and
    /// are not drawn (Ctrl+Shift+Z)
    zoomed: bool,
    /// the group this tab belongs to (`groups.rs`); the tab bar shows
    /// only the active tab's group
    group: u32,
}

impl Tab {
    /// What the tab bar, the cockpit and a toast call this tab: the user's
    /// name when there is one, else the focused pane's folder. Broadcast
    /// is marked in the label so it can't be left on by accident.
    fn label(&self) -> String {
        tab_label(
            self.name.as_deref().unwrap_or(&self.title),
            self.broadcast,
            if self.zoomed {
                self.panes.len().saturating_sub(1)
            } else {
                0
            },
        )
    }

    /// The tab's progress: its focused pane's, else any pane's — a
    /// background pane's Claude is still this tab's work.
    fn progress(&self) -> Option<term::Progress> {
        self.panes
            .get(self.focused)
            .and_then(|p| p.progress)
            .or_else(|| self.panes.iter().find_map(|p| p.progress))
    }

    fn busy(&self, now_active: bool) -> bool {
        !now_active
            && self.panes.iter().any(|p| {
                p.last_output
                    .is_some_and(|ts| ts.elapsed() < std::time::Duration::from_secs(2))
            })
    }
}

/// Pane rects with the focused pane zoomed: it gets the whole content
/// rect and every other pane an empty one. An empty rect is not drawn, is
/// not hit by the pointer, and is skipped by relayout, so a hidden pane
/// keeps its PTY size and its program never sees a resize to nothing.
fn zoom_rects(content: render::Rect, panes: usize, focused: usize) -> Vec<render::Rect> {
    (0..panes)
        .map(|i| {
            if i == focused {
                content
            } else {
                (content.0, content.1, 0, 0)
            }
        })
        .collect()
}

/// A rect that holds at least one cell's worth of pixels. Zoomed-away
/// panes have none.
fn rect_visible(rect: &render::Rect) -> bool {
    rect.2 > 0 && rect.3 > 0
}

/// A tab's label: `» ` while broadcasting, and ` +N` while zoomed, N being
/// the panes the zoom hides. Both states change where keys go or what is
/// on screen, so neither may be invisible.
fn tab_label(base: &str, broadcast: bool, hidden: usize) -> String {
    let mut label = String::new();
    if broadcast {
        label.push_str("» ");
    }
    label.push_str(base);
    if hidden > 0 {
        label.push_str(&format!(" +{hidden}"));
    }
    label
}

/// The top-right bar: the search query while searching (search owns the
/// keys then, even in copy mode), else the copy-mode hint, else nothing.
fn bar_text(search: Option<&str>, copy_mode: bool) -> Option<String> {
    match search {
        Some(q) => Some(format!("{}: {q}_", tr("بحث", "Search"))),
        None => copy_mode.then(|| copymode::hint().to_string()),
    }
}

/// Which panes besides the focused one hear typed input. Broadcast with a
/// single pane is nothing extra.
fn broadcast_targets(on: bool, panes: usize, focused: usize) -> Vec<usize> {
    if !on {
        return Vec::new();
    }
    (0..panes).filter(|&i| i != focused).collect()
}

/// Global rows `[start, end)` to the grid lines of their first and last
/// row, for the current history size. Rows already pushed out of the
/// scrollback are dropped from the top; nothing left is `None`.
fn output_lines(
    start: u64,
    end: u64,
    evicted: u64,
    history: u64,
    screen_lines: usize,
) -> Option<(i32, i32)> {
    let to_line = |abs: u64| abs as i64 - evicted as i64 - history as i64;
    let first = to_line(start).max(-(history as i64));
    let last = to_line(end.checked_sub(1)?).min(screen_lines as i64 - 1);
    (end > start && first <= last).then_some((first as i32, last as i32))
}

/// Longest tab name kept, in characters. The tab bar truncates further.
const TAB_NAME_MAX: usize = 40;

/// The name a user typed, made safe to draw: trimmed, without control
/// characters or bidi embeddings/overrides/isolates (a name must not be
/// able to reorder the tab bar around it), and capped. Empty is no name.
fn clean_tab_name(raw: &str) -> Option<String> {
    let kept: String = raw
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
        .take(TAB_NAME_MAX)
        .collect();
    let name = kept.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Top-level axis and weights of a tree, for the one-axis fallback and for
/// readers that predate the nested `layout` field. A leaf has no split.
fn saved_split(layout: &layout::Node) -> (u8, Vec<f32>) {
    match layout {
        layout::Node::Split { axis, weights, .. } => {
            let orientation = u8::from(*axis == Orientation::Column);
            (orientation, weights.clone())
        }
        layout::Node::Leaf(_) => (0, Vec::new()),
    }
}

/// A saved tab's layout for the `n` panes that actually came back: its tree
/// when it holds exactly those panes, else the old one-axis form (files from
/// before nested splits, or a tab where a pane failed to respawn).
fn restored_layout(st: &SavedTab, n: usize) -> layout::Node {
    match &st.layout {
        Some(tree) if tree.valid_for(n) => tree.clone(),
        _ => {
            let axis = if st.orientation == 1 {
                Orientation::Column
            } else {
                Orientation::Row
            };
            layout::Node::flat(axis, &st.weights, n)
        }
    }
}

/// How a saved tab may attach after its first shell is spawned.
///
/// `apply` is false when that shell never started: the extra directories,
/// the layout, the focus and the name stay with the saved tab and are not
/// written onto whichever tab happens to be last (N-05). `extra` is how
/// many further panes may be spawned without passing `MAX_PANES_PER_TAB`.
struct RestoredTabPlan {
    extra: usize,
    apply: bool,
}

fn plan_restored_tab(
    first_shell_started: bool,
    panes_now: usize,
    extra_cwds: usize,
) -> RestoredTabPlan {
    if !first_shell_started {
        return RestoredTabPlan {
            extra: 0,
            apply: false,
        };
    }
    RestoredTabPlan {
        extra: extra_cwds.min(MAX_PANES_PER_TAB.saturating_sub(panes_now)),
        apply: true,
    }
}

/// What survives a restart: every tab's FULL layout — each pane's cwd, the
/// split axis and weights, and which pane was focused.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedTab {
    cwds: Vec<Option<String>>,
    #[serde(default)]
    weights: Vec<f32>,
    /// 0 = Row (side by side), 1 = Column (stacked)
    #[serde(default)]
    orientation: u8,
    #[serde(default)]
    focused: usize,
    /// the split tree; absent in files from before nested splits, which
    /// restore from `orientation` + `weights` as one row or column
    #[serde(default, skip_serializing_if = "Option::is_none")]
    layout: Option<layout::Node>,
    /// the user's tab name; absent in files from before M38
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// index into `SavedState::groups`; absent (0, the first group) in
    /// files from before tab groups
    #[serde(default, skip_serializing_if = "is_zero")]
    group: usize,
    /// each pane's shell, beside `cwds`; `None` is the default shell (so a
    /// changed default applies). Absent in files from before this field.
    /// Only `shells::is_restorable` commands are started again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    shells: Vec<Option<String>>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedState {
    tabs: Vec<SavedTab>,
    active: usize,
    /// each group's name (None: numbered), in order; empty in files from
    /// before tab groups, which restore as one group
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    groups: Vec<Option<String>>,
    /// where the window was, so it comes back there (WezTerm's most-asked
    /// small thing). Physical pixels; absent in files from before M34.
    #[serde(default)]
    window: Option<WindowGeom>,
}

/// The window's last NON-maximized rectangle plus whether it was maximized —
/// so restoring a maximized window also knows where "restore down" goes.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct WindowGeom {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    #[serde(default)]
    maximized: bool,
}

/// A saved rectangle is worth restoring only if enough of it lands on a
/// monitor that is still connected: a window last seen on an unplugged
/// second screen must not come back off-screen with no way to grab it.
/// `monitors` are (x, y, w, h) in physical pixels.
fn geometry_on_screen(g: WindowGeom, monitors: &[(i32, i32, u32, u32)]) -> bool {
    const NEED: i64 = 120; // px of the window's top strip that must be reachable
    if g.w < 200 || g.h < 120 {
        return false;
    }
    let (gx0, gy0) = (g.x as i64, g.y as i64);
    let (gx1, gy1) = (gx0 + g.w as i64, gy0 + 64); // the title strip, where you grab it
    monitors.iter().any(|&(mx, my, mw, mh)| {
        let (mx0, my0, mx1, my1) = (
            mx as i64,
            my as i64,
            mx as i64 + mw as i64,
            my as i64 + mh as i64,
        );
        let ox = gx1.min(mx1) - gx0.max(mx0);
        let oy = gy1.min(my1) - gy0.max(my0);
        ox >= NEED && oy >= 32
    })
}

/// Read and parse ~/.bayan/session.json (None when absent or corrupt).
fn load_saved_state() -> Option<SavedState> {
    state_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| parse_state(&s))
}

/// The pre-M9 file format (one cwd per tab) — read it rather than dropping
/// the user's layout on upgrade.
#[derive(serde::Deserialize)]
struct LegacyState {
    tabs: Vec<Option<String>>,
    active: usize,
}

fn parse_state(json: &str) -> Option<SavedState> {
    if let Ok(s) = serde_json::from_str::<SavedState>(json) {
        return Some(s);
    }
    let legacy: LegacyState = serde_json::from_str(json).ok()?;
    Some(SavedState {
        tabs: legacy
            .tabs
            .into_iter()
            .map(|cwd| SavedTab {
                cwds: vec![cwd],
                weights: vec![1.0],
                orientation: 0,
                focused: 0,
                layout: None,
                name: None,
                group: 0,
                shells: Vec::new(),
            })
            .collect(),
        active: legacy.active,
        groups: Vec::new(),
        window: None,
    })
}

/// Each pane's shell for session.json: `None` where it is the default, and
/// an empty list when every pane is, so an ordinary session file does not
/// change shape.
fn saved_shells<'a>(commands: impl Iterator<Item = &'a str>, default: &str) -> Vec<Option<String>> {
    let v: Vec<Option<String>> = commands
        .map(|c| (c != default).then(|| c.to_string()))
        .collect();
    if v.iter().all(Option::is_none) {
        Vec::new()
    } else {
        v
    }
}

/// The shell a restored pane starts with: its saved one if `detect` could
/// have offered it, otherwise the default.
fn restored_shell(saved: Option<String>, default: &str, installed: &[shells::Shell]) -> String {
    saved
        .filter(|c| shells::is_restorable(c, installed))
        .unwrap_or_else(|| default.to_string())
}

fn state_path() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(path) = SESSION_FILE.with(|slot| slot.borrow().clone()) {
        return Some(path);
    }
    let home = std::env::var("USERPROFILE").ok()?;
    Some(
        std::path::Path::new(&home)
            .join(".bayan")
            .join("session.json"),
    )
}

#[cfg(test)]
std::thread_local! {
    static SESSION_FILE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Multi-click detection: 1 = simple, 2 = word (semantic), 3 = line.
struct ClickTracker {
    last: Option<(Instant, (i32, usize))>,
    count: u8,
}

impl ClickTracker {
    fn new() -> Self {
        Self {
            last: None,
            count: 0,
        }
    }

    fn click(&mut self, now: Instant, cell: (i32, usize)) -> u8 {
        const DOUBLE_MS: u128 = 400;
        self.count = match self.last {
            Some((t, c)) if now.duration_since(t).as_millis() <= DOUBLE_MS && c == cell => {
                if self.count >= 3 {
                    1
                } else {
                    self.count + 1
                }
            }
            _ => 1,
        };
        self.last = Some((now, cell));
        self.count
    }
}

#[derive(Default)]
struct SearchState {
    query: String,
    hl: Option<Match>,
    /// A seeded selection searches the exact source cells while the field
    /// shows readable logical Arabic. Editing returns to ordinary search.
    selected_needle: Option<String>,
}

impl SearchState {
    fn from_selection(text: Option<&str>, claude: bool) -> Self {
        let query = selection_search_query(text, claude);
        let selected_needle = (!query.is_empty()).then(|| text.unwrap_or_default().to_string());
        Self {
            query,
            hl: None,
            selected_needle,
        }
    }
}

fn selection_search_query(text: Option<&str>, claude: bool) -> String {
    let Some(text) = text else {
        return String::new();
    };
    if text.chars().any(char::is_control) || text.chars().count() > 256 {
        return String::new();
    }
    if claude {
        bidi::restore_block(text)
    } else {
        text.to_string()
    }
}

fn keyboard_zoom(
    key: winit::keyboard::PhysicalKey,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<i32> {
    use winit::keyboard::{KeyCode, PhysicalKey};
    if !ctrl || alt {
        return None;
    }
    match key {
        PhysicalKey::Code(KeyCode::Equal | KeyCode::NumpadAdd) => Some(1),
        PhysicalKey::Code(KeyCode::Minus) if !shift => Some(-1),
        PhysicalKey::Code(KeyCode::NumpadSubtract) => Some(-1),
        _ => None,
    }
}

/// One row of the command palette. Built-ins run through `perform_action`,
/// the same path a keybinding takes, so the palette can never drift from
/// what the shortcuts do.
#[derive(Clone, PartialEq, Eq)]
enum PaletteRow {
    Builtin(keybinds::Action),
    /// index into `plugins::registry().actions`
    Plugin(usize),
    Shell(String),
    CopyText(String),
    InsertCommand(&'static str),
    /// name the active tab; `None` gives it back its folder name
    RenameTab(Option<String>),
    /// show this group (at the tab it was last on)
    ShowGroup(u32),
    /// move the active tab into this group; `None` makes a new one
    MoveToGroup(Option<u32>),
    /// open the palette in rename-the-group mode
    OpenRenameGroup,
    /// open the shell menu (from the pane's right-click menu)
    OpenShellMenu,
    /// name the active group; `None` gives it back its number
    RenameGroup(Option<String>),
    /// open the SSH host list
    OpenSsh,
    /// one host from that list (the token ssh receives after `--`)
    SshHost(String),
    /// detach the focused mux session
    DetachSession,
    /// list sessions the server is still running
    OpenDetached,
    /// bring a detached session back into a tab
    Reattach(u64),
    /// end a detached session
    KillSession(u64),
}

/// Rows of the right-click menu, most used first. Each one is a keymap
/// action, so a row does exactly what its shortcut does.
fn context_menu_rows(
    has_selection: bool,
    panes: usize,
    zoomed: bool,
    broadcast: bool,
) -> Vec<(PaletteRow, String)> {
    use keybinds::Action::*;
    let mut rows = Vec::new();
    let mut add = |a: keybinds::Action, label: &str| {
        rows.push((PaletteRow::Builtin(a), label.to_string()));
    };
    if has_selection {
        add(Copy, Copy.label());
        add(CopyClean, CopyClean.label());
    }
    for a in [Paste, CopyLastOutput, Search, SplitH, SplitV] {
        add(a, a.label());
    }
    if panes > 1 {
        add(
            ZoomPane,
            if zoomed {
                tr("إلغاء تكبير اللوحة", "Unzoom pane")
            } else {
                ZoomPane.label()
            },
        );
        add(
            Broadcast,
            if broadcast {
                tr("إيقاف الكتابة في كل اللوحات", "Stop typing to all panes")
            } else {
                Broadcast.label()
            },
        );
    }
    add(NewTab, NewTab.label());
    rows.push((
        PaletteRow::OpenShellMenu,
        tr("تبويب جديد بطرفية…", "New tab with shell…").to_string(),
    ));
    for a in [NewGroup, RenameTab, ClosePane, Settings, BidiToggle] {
        rows.push((PaletteRow::Builtin(a), a.label().to_string()));
    }
    rows
}

/// The shell menu's rows: every detected shell, in detection order
/// (PowerShell, PowerShell 7, cmd, Git Bash, each WSL distro), the default
/// for new tabs marked. Detection runs on a background thread at startup;
/// until it reports, the two shells every Windows has stand in.
fn shell_menu_rows(profiles: &[shells::Shell], default: &str) -> Vec<(PaletteRow, String)> {
    let listed: Vec<(String, String)> = if profiles.is_empty() {
        [
            ("PowerShell", "powershell.exe"),
            ("Command Prompt", "cmd.exe"),
        ]
        .iter()
        .map(|(name, cmd)| (name.to_string(), cmd.to_string()))
        .collect()
    } else {
        profiles
            .iter()
            .map(|s| (s.label(), s.command.clone()))
            .collect()
    };
    listed
        .into_iter()
        .map(|(name, command)| {
            let label = if command == default {
                format!("{name} · {}", tr("الافتراضية", "default"))
            } else {
                name
            };
            (PaletteRow::Shell(command), label)
        })
        .collect()
}

/// The Arabic-font cycler's step: the ring is Auto (`None`), then each of
/// `choices` in order, wrapping both ways. An unknown current counts as
/// Auto.
fn step_arabic_font(choices: &[String], current: Option<&str>, dir: i32) -> Option<String> {
    let n = choices.len() as i32 + 1; // + Auto at position 0
    let at = current
        .and_then(|c| choices.iter().position(|x| x == c))
        .map_or(0, |i| i as i32 + 1);
    let next = (at + dir).rem_euclid(n);
    (next > 0).then(|| choices[next as usize - 1].clone())
}

/// What the rename palette names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenameTarget {
    Tab,
    Group,
}

/// The palette's rows while naming a tab or a group: the typed name first,
/// so Enter takes it, then the way back to the default label (the folder
/// name for a tab, the number for a group).
fn rename_rows(target: RenameTarget, query: &str) -> Vec<(PaletteRow, String)> {
    let mut rows = Vec::new();
    let name = clean_tab_name(query);
    match target {
        RenameTarget::Tab => {
            if let Some(name) = name {
                rows.push((
                    PaletteRow::RenameTab(Some(name.clone())),
                    if lang::rtl() {
                        format!("تسمية التبويب «{name}»")
                    } else {
                        format!("Name the tab “{name}”")
                    },
                ));
            }
            rows.push((
                PaletteRow::RenameTab(None),
                tr("إعادة اسم المجلد", "Back to the folder name").to_string(),
            ));
        }
        RenameTarget::Group => {
            if let Some(name) = name {
                rows.push((
                    PaletteRow::RenameGroup(Some(name.clone())),
                    if lang::rtl() {
                        format!("تسمية المجموعة «{name}»")
                    } else {
                        format!("Name the group “{name}”")
                    },
                ));
            }
            rows.push((
                PaletteRow::RenameGroup(None),
                tr("إعادة رقم المجموعة", "Back to the group number").to_string(),
            ));
        }
    }
    rows
}

/// The palette's group rows: show another group, move the active tab
/// into one (or into a new group), and name the group on screen. `groups`
/// is (id, label) in order; nothing to switch to or move into with one
/// group, so only the move-to-new row shows then. A move that would leave
/// the tab where it is is not offered.
fn group_rows(groups: &[(u32, String)], active: u32) -> Vec<(PaletteRow, String)> {
    let mut rows = Vec::new();
    for (id, label) in groups.iter().filter(|(id, _)| *id != active) {
        rows.push((
            PaletteRow::ShowGroup(*id),
            format!("{}: {label}", tr("المجموعة", "Group")),
        ));
    }
    for (id, label) in groups.iter().filter(|(id, _)| *id != active) {
        rows.push((
            PaletteRow::MoveToGroup(Some(*id)),
            if lang::rtl() {
                format!("نقل التبويب إلى «{label}»")
            } else {
                format!("Move tab to “{label}”")
            },
        ));
    }
    rows.push((
        PaletteRow::MoveToGroup(None),
        tr("نقل التبويب إلى مجموعة جديدة", "Move tab to a new group").to_string(),
    ));
    if groups.len() > 1 {
        rows.push((
            PaletteRow::OpenRenameGroup,
            tr("تسمية المجموعة", "Rename group").to_string(),
        ));
    }
    rows
}

/// Longest PTY-influenced string the window chrome draws, in chars.
const CHROME_TEXT_MAX: usize = 120;

/// BAYAN-012: one sanitizer for every string the chrome draws that a program
/// can shape (the cockpit's `▶ cmd`, its idle cwd, the close guard's command).
/// Control, bidi and zero-width characters (bar the Persian joiners) become
/// visible markers (`<RLO>`)
/// via `link::preview`, the same treatment the link cards give, and the text
/// is capped. A spoofed RLO can no longer make the row say something else.
fn chrome_text(raw: &str) -> String {
    // the path form keeps ZWNJ/ZWJ: a Persian folder or command name is
    // spelled with them, and they cannot reorder the row
    link::preview_with(raw, CHROME_TEXT_MAX, link::is_forbidden_in_path)
}

/// Counted form of «لوحة», the same agreement as the paste card's lines.
fn arabic_panes(n: usize) -> String {
    match n {
        2 => "لوحتين".to_string(),
        3..=10 => format!("{n} لوحات"),
        _ => format!("{n} لوحة"),
    }
}

/// The card's note when Enter will also type into `others` broadcast panes.
/// `others` is the extra panes, so the note counts the focused one too.
fn broadcast_note(others: usize) -> Option<String> {
    let n = others + 1;
    (n >= 2).then(|| {
        if lang::rtl() {
            format!(" — يُرسل إلى {}", arabic_panes(n))
        } else {
            format!(" — goes to {n} panes")
        }
    })
}

/// Whether a card raised for `raised_for` may still send (R-02): only into
/// the same pane. A card with no pane (no tab was open) sends nowhere.
fn card_still_targets(raised_for: Option<u64>, focused: Option<u64>) -> bool {
    raised_for.is_some() && raised_for == focused
}

/// Drop a confirm card whose pane is one of `removed`.
///
/// The card is painted over the whole window. Closing that pane, closing
/// its tab, or the shell exiting otherwise leaves it up on top of a
/// different tab, where Enter does nothing (`card_still_targets` already
/// refuses the send). A card for a pane that is still open stays. A card
/// with no pane id matches nothing.
///
/// `blocked_link` is not cleared here. It is not keyed by a pane: it is a
/// window-level refuse or confirm for a URL, and closing a shell must not
/// dismiss it.
fn dismiss_paste_for(pending: &mut Option<PendingPaste>, removed: &[u64]) {
    if pending
        .as_ref()
        .is_some_and(|p| p.pane.is_some_and(|id| removed.contains(&id)))
    {
        *pending = None;
    }
}

/// Chars of preview per card row; the card is 62 cells wide.
const CARD_ROW_CHARS: usize = 56;
/// Visible rows of a Lua approval card. The rest scroll.
const LUA_CARD_ROWS: usize = 12;
/// After Enter reveals a script, the next Enter is ignored until this
/// passes, so a held key cannot approve on autorepeat.
const LUA_APPROVE_DELAY: std::time::Duration = std::time::Duration::from_millis(700);

/// Change one action on the saved map and the same action on the runtime
/// map. A chord that exists only on the runtime map — a `config.lua`
/// binding — is left where it is.
fn apply_shortcut(
    saved: &mut Option<std::collections::HashMap<String, String>>,
    runtime: &mut Option<std::collections::HashMap<String, String>>,
    action_id: &str,
    chord: Option<String>,
) {
    put_binding(saved, action_id, chord.clone());
    put_binding(runtime, action_id, chord);
}

fn put_binding(
    map: &mut Option<std::collections::HashMap<String, String>>,
    action_id: &str,
    chord: Option<String>,
) {
    match chord {
        Some(c) => {
            map.get_or_insert_with(Default::default)
                .insert(action_id.to_string(), c);
        }
        None => {
            let empty = map.as_mut().is_some_and(|m| {
                m.remove(action_id);
                m.is_empty()
            });
            if empty {
                *map = None;
            }
        }
    }
}

/// The rows the approval card paints, and whether the last row is in view.
fn lua_card_window(rows: &[String], scroll: usize) -> (Vec<String>, usize, bool) {
    if rows.len() <= LUA_CARD_ROWS {
        return (rows.to_vec(), 0, true);
    }
    let max = rows.len() - LUA_CARD_ROWS;
    let scroll = scroll.min(max);
    let at_end = scroll >= max;
    (
        rows[scroll..scroll + LUA_CARD_ROWS].to_vec(),
        scroll,
        at_end,
    )
}

fn lua_enter_ready(not_before: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    not_before.is_none_or(|t| now >= t)
}

/// The card's preview rows. A paste shows one elided row, as before. A
/// plugin command shows ALL of what Enter will type (R-03), wrapped into
/// rows, with CR as ␍. The 512-char cap stays within ten rows because
/// plugin load rejects every character this preview would expand.
fn card_preview(kind: PendingKind, text: &str) -> Vec<String> {
    // A plugin command and a Lua file show every character the card is
    // holding (R-03). A paste stays one elided row.
    if !matches!(kind, PendingKind::Command | PendingKind::ApproveLua) {
        return vec![term::paste_preview(text, 48)];
    }
    let full = term::paste_preview(text, usize::MAX);
    let chars: Vec<char> = full.chars().collect();
    let rows: Vec<String> = chars
        .chunks(CARD_ROW_CHARS)
        .map(|c| c.iter().collect())
        .collect();
    if rows.is_empty() {
        vec![String::new()]
    } else {
        rows
    }
}

/// The shortcut shown beside a palette or menu row: the live chord of a
/// built-in action, nothing for anything else.
fn row_shortcut(keymap: &[(keybinds::Action, keybinds::Chord)], row: &PaletteRow) -> String {
    match row {
        PaletteRow::Builtin(a) => keymap
            .iter()
            .find(|(act, _)| act == a)
            .map(|(_, c)| c.display())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// The palette text for a plugin action: always badged «إضافة», so a plugin
/// row is told apart from Bayan's own at a glance (BAYAN-007).
fn plugin_row_label(label: &str) -> String {
    format!("{} — {label}", tr("إضافة", "Plugin"))
}

/// What a plugin action holds for the confirm card: its text is never typed
/// without one (BAYAN-007).
fn plugin_pending(label: &str, send: &str, pane: Option<u64>) -> PendingPaste {
    let (lines, chars) = match term::needs_paste_guard(send) {
        Some(g) => (g.lines, g.chars),
        None => (1, send.chars().count()),
    };
    PendingPaste {
        kind: PendingKind::Command,
        pane,
        text: send.to_string(),
        lines,
        chars,
        reason: format!(
            "{} — {label}",
            tr("إضافة تريد كتابة نص", "A plugin wants to type text")
        ),
        lua: None,
        scroll: 0,
        saw_end: true,
    }
}

fn palette_extras(
    profiles: &[shells::Shell],
    links: &[link::LinkSpan],
) -> Vec<(PaletteRow, String)> {
    let mut rows: Vec<_> = profiles
        .iter()
        .map(|shell| {
            (
                PaletteRow::Shell(shell.command.clone()),
                format!("{} — {}", tr("تبويب جديد", "New tab"), shell.label()),
            )
        })
        .collect();
    rows.push((
        PaletteRow::InsertCommand("claude"),
        tr("إدراج أمر كلاود — claude", "Insert Claude command — claude").into(),
    ));
    rows.push((
        PaletteRow::InsertCommand("claude --continue"),
        tr(
            "متابعة محادثة كلاود — claude --continue",
            "Continue Claude conversation — claude --continue",
        )
        .into(),
    ));
    let mut seen = std::collections::HashSet::new();
    for span in links {
        let text = match &span.kind {
            link::LinkKind::Osc { uri, .. } | link::LinkKind::Url { uri } => uri,
            link::LinkKind::File { path, .. } => path,
        };
        if text.is_empty() || !seen.insert(text.clone()) {
            continue;
        }
        let shown = link::fit_status_bar(&span.tooltip(None), 90).text();
        rows.push((
            PaletteRow::CopyText(text.clone()),
            format!("{} — {shown}", tr("نسخ", "Copy")),
        ));
        if seen.len() >= 100 {
            break;
        }
    }
    rows
}

/// Ctrl+Shift+P: type to filter, arrows to move, Enter to run.
struct Palette {
    query: String,
    /// naming the active tab or its group: the query is the new name, not
    /// a filter
    rename: Option<RenameTarget>,
    /// the right-click menu: a popup at this window point instead of the
    /// centred palette, listing `context_rows`
    menu: Option<(f64, f64)>,
    /// the menu lists the installed shells (`shell_menu_rows`) instead of
    /// the pane actions: a right-click on the tab bar
    shells_menu: bool,
    /// the rows are SSH hosts
    ssh_menu: bool,
    /// the shell menu was opened from the tab-bar arrow (not a right-click).
    /// SSH and the mux are rows of that menu only, and only when this
    /// window is not elevated.
    shell_arrow: bool,
    /// the rows are detached mux sessions
    detached_menu: bool,
    sel: usize,
    extras: Vec<(PaletteRow, String)>,
    pressed: Option<(usize, PaletteRow)>,
    wheel: f32,
}

/// A frame's snapshot of the palette: (query, row labels, selected row,
/// the right-click anchor when it is the menu).
type PaletteView = (String, Vec<render::MenuRow>, usize, Option<(f64, f64)>);

/// A frame's snapshot of the picker: (names, previews, selected row).
type PoshView = (Vec<String>, Vec<Option<Vec<u8>>>, usize);

/// The prompt picker: theme names with LIVE previews rendered by oh-my-posh
/// itself, so what you see is the prompt you get.
struct PoshPicker {
    names: Vec<String>,
    /// preview ANSI per name, filled in by the `bayan-posh` worker as each
    /// subprocess finishes (None = not rendered yet, or no preview)
    previews: Vec<Option<Vec<u8>>>,
    /// a row already handed to the worker. `previews` stays None both while
    /// that run is in flight and when the theme has no preview, so without
    /// this a second highlight starts oh-my-posh again.
    asked: Vec<bool>,
    sel: usize,
    /// which open this is: a preview from an earlier, closed picker is dropped
    generation: u64,
    /// the worker's queue: (row, theme, eager)
    work: std::sync::mpsc::Sender<(usize, String, bool)>,
}

/// Bumped on every picker open and close; the preview worker stops as soon
/// as it no longer matches its own generation (BAYAN-006).
static POSH_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether highlight `i` still needs a preview subprocess. A row that was
/// already asked, or that already has ANSI, does not.
fn claim_posh_preview(asked: &mut [bool], have_preview: bool, i: usize) -> bool {
    if have_preview || asked.get(i).copied().unwrap_or(true) {
        return false;
    }
    asked[i] = true;
    true
}

/// One burst of preview requests, as the worker runs it (R-04): every eager
/// row (featured + selected on open), then only the NEWEST highlight request.
/// The second list is highlight rows that will not run. The picker clears
/// their claims, so coming back asks again instead of leaving the row blank.
fn collapse_posh_requests(
    burst: Vec<(usize, String, bool)>,
) -> (Vec<(usize, String, bool)>, Vec<usize>) {
    let last_hl = burst.iter().rposition(|r| !r.2);
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for (k, row) in burst.into_iter().enumerate() {
        if row.2 || Some(k) == last_hl {
            kept.push(row);
        } else {
            dropped.push(row.0);
        }
    }
    // A row that also runs (the newest highlight, or an eager copy of it)
    // is not dropped: clearing that claim would start oh-my-posh twice.
    dropped.retain(|i| !kept.iter().any(|row| row.0 == *i));
    (kept, dropped)
}

/// Clear claims the worker did not run. A row whose preview already arrived
/// stays claimed. Returns whether `sel` was released and still has nothing
/// to show, so the picker can ask again without another key.
fn release_dropped_posh(
    asked: &mut [bool],
    arrived: &[bool],
    dropped: &[usize],
    sel: usize,
) -> bool {
    let mut again = false;
    for &j in dropped {
        if arrived.get(j).copied().unwrap_or(false) {
            continue;
        }
        let Some(slot) = asked.get_mut(j) else {
            continue;
        };
        if *slot && j == sel {
            again = true;
        }
        *slot = false;
    }
    again
}

struct App {
    /// the prompt picker, when open
    posh: Option<PoshPicker>,
    /// when a PTY-triggered toast last fired — rate-limits program output
    /// that asks for attention in a loop
    last_toast: Option<Instant>,
    /// most-recently-used tab history for Ctrl+Tab
    mru: mru::Mru,
    /// tab groups in display order; each tab names its own (`groups.rs`)
    groups: groups::Groups,
    /// the group the next spawned tab joins instead of the active one —
    /// set only for the one spawn that creates or restores a group
    spawn_group: Option<u32>,
    /// the command palette, when open
    palette: Option<Palette>,
    /// The shell-menu arrow has keyboard focus. Esc from the menu sets
    /// this; Enter on the arrow opens the menu; a pick clears it so the
    /// new tab takes focus.
    shell_arrow_focused: bool,
    /// Live connection to `bayan --mux-server`. Absent unless `mux.enabled`
    /// and a mux action has already needed it. Closing the window does not
    /// kill this process when a session was detached.
    mux_client: Option<mux::MuxClient>,
    shell_profiles: Vec<shells::Shell>,
    shell_profiles_ready: bool,
    startup_pending: bool,
    /// last click on the empty title strip — powers double-click-to-maximise
    last_strip_click: Option<std::time::Instant>,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    gpu: Option<gpu::Gpu>,
    renderer: Option<render::Renderer>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    size: TermSize,
    modifiers: ModifiersState,
    first_frame: bool,
    first_output: bool,
    /// last frame's quad bytes — an identical frame skips the GPU submit
    /// (a still terminal stops re-rendering; laptop battery thanks you)
    last_frame: Vec<u8>,
    last_frame_size: Option<winit::dpi::PhysicalSize<u32>>,
    cursor_pos: PhysicalPosition<f64>,
    /// The cursor box last given to `set_ime_cursor_area`.
    ime_spot: Option<(i32, i32, i32, i32)>,
    /// The IME context was rebuilt (`Ime::Enabled`) or the window focused
    /// again. Windows forgot the composition window; send the same box
    /// again even when it has not moved (winit#2569).
    ime_area_reset: bool,
    /// Grab, motion dedupe, focus-in memory, and the local selection drag.
    /// A focus loss drops all of them together.
    mouse: mouse::Route,
    /// the pane a mouse selection started in (drags don't cross panes)
    sel_pane: usize,
    /// divider being dragged (between pane i and i+1)
    drag_divider: Option<usize>,
    /// Per-pane scrollbar: hover, thumb drag, and when it was last scrolled.
    bars: scrollbar::Bars,
    /// the agent cockpit overlay (Ctrl+Shift+D): one glance at every tab
    cockpit: bool,
    cockpit_sel: usize,
    /// a paste or a drop awaiting Enter/Esc (EasyTer's paste guard)
    pending_paste: Option<PendingPaste>,
    /// a close awaiting Enter/Esc (a command is still running)
    pending_close: Option<CloseTarget>,
    swallow_guard_release: bool,
    /// link under the pointer: underline + the target in the pane status
    link_hover: Option<LinkHover>,
    /// a refused link, or a userinfo URL waiting for Enter.
    blocked_link: Option<BlockedLink>,
    /// Link spans per pane row. Invalidated on PTY damage, scroll, relayout.
    link_cache: link::SpanCache,
    /// last keystroke sent to a PTY — the cursor blinks for 15s after it,
    /// then parks solid so an idle window stops re-rendering (M14's win)
    last_input: Instant,
    /// the blink phase the last frame actually drew (change → redraw)
    blink_drawn_on: bool,
    /// the settings panel (Ctrl+,) with its selected row, when open
    settings: Option<usize>,
    /// window focus (finish notifications only fire when nobody's looking)
    focused: bool,
    clicks: ClickTracker,
    wheel_accum: f32,
    /// Per-pane sub-line scroll. Empty unless `scroll.smooth` is on, so a
    /// disabled wheel never shifts a row.
    smooth: std::collections::HashMap<u64, scroll::PaneScroll>,
    /// Last time an easing notch was sampled. None while nothing is pending.
    smooth_at: Option<Instant>,
    /// Last alt-screen bit seen per pane, so a transition clears the offset
    /// even when no wheel notch is still easing.
    alt_seen: std::collections::HashMap<u64, bool>,
    search: Option<SearchState>,
    clipboard: Option<arboard::Clipboard>,
    /// Claude mode follows (alt screen + claude command) automatically;
    /// F2 switches to manual and back (EasyTer's toggle semantics).
    auto_follow: bool,
    claude_manual: bool,
    config: config::UserConfig,
    /// Pixel budget shared by every pane. Caps are the ones from config at
    /// startup; a later edit applies to the next launch, like scrollback.
    image_budget: Arc<images::ImageBudget>,
    /// The config.json document. Lua overrides live only in `config`.
    json_config: config::UserConfig,
    config_load_error: Option<String>,
    startup_error: Option<String>,
    /// Executable Lua plugins that passed the allowlist and the hash approval.
    lua_plugins: lua_host::PluginSet,
    /// config.lua and plugins the confirm card has not approved yet.
    lua_pending: Vec<lua_host::PendingScript>,
    /// Hashes dismissed this session, so a reload does not raise them again.
    lua_skipped: Vec<String>,
    /// Text a Lua callback wants typed. Shown on the plugin confirm card.
    lua_send_queue: std::collections::VecDeque<lua_host::PluginSend>,
    /// Lua failures to show once the window exists. Never fatal.
    lua_errors: Vec<String>,
    /// Set when the token is elevated and scripts were left unloaded.
    lua_notice: Option<String>,
    /// Enter on a revealed Lua card is ignored until this instant.
    lua_approve_not_before: Option<std::time::Instant>,
    /// every rebindable action's effective chord (defaults + config)
    keymap: Vec<(keybinds::Action, keybinds::Chord)>,
    /// the shortcuts editor overlay, when open
    shortcuts: Option<ShortcutsState>,
    /// last seen atlas generation (a reset schedules a healing redraw)
    atlas_generation: u32,
    /// Ctrl+wheel zoom, in points on top of the configured size.
    font_delta: f32,
    renderer_building: bool,
    /// Last High Contrast / text-size read. The renderer in flight was
    /// built from `inflight_a11y`; a change during the build is chased.
    a11y: a11y::A11ySnapshot,
    inflight_a11y: a11y::A11ySnapshot,
    /// the last rectangle the window had while NOT maximized — what a
    /// maximized window restores down to, and what the session saves
    normal_geom: Option<WindowGeom>,
    /// Physical keys whose press never reached the PTY. The matching release
    /// must not either: a TUI that asked for key-up events would otherwise
    /// see a release whose press it never got. Copy clears the selection
    /// before the key comes up, and a chord can open a panel, so this is
    /// recorded at press time rather than re-derived when the key is released.
    keys_held_away: Vec<winit::keyboard::PhysicalKey>,
    /// BAYAN_PASTE's text, waiting for the first renderer: a right click
    /// needs cell metrics to land on a pane.
    debug_paste: Option<String>,
    /// `--cwd`'s folder, already gated, until the first tab opens in it.
    /// `#45`: with saved tabs restored, they come back first and this folder
    /// opens as the active extra tab.
    start_dir: Option<String>,
    /// UI Automation adapter. `None` until the window exists. The first
    /// client turns it on for the rest of the session on Windows.
    reader: Option<accesskit_winit::Adapter>,
    /// Set when a client has asked for the tree. Before that, redraws do
    /// not stamp or build. After that, [`screen_reader::TreePace`] decides.
    reader_seen_client: bool,
    /// Bumped when pane text changes without the cursor or the selection
    /// necessarily moving (a PTY chunk, a relayout).
    reader_gen: u64,
    reader_pace: screen_reader::TreePace,
    /// Trailing tree build, when a stamp changed inside the 10 Hz window.
    reader_tree_at: Option<Instant>,
    /// Epoch for the announcer's millisecond clock.
    reader_origin: Instant,
    /// When the held output burst may be spoken. `about_to_wait` wakes then.
    reader_deadline: Option<Instant>,
    reader_gate: screen_reader::OutputGate,
    /// Document text last given to the tree. Held still across a flood.
    reader_bodies: Vec<screen_reader::PaneItem>,
    /// Focused pane the gate is diffing. A change resets the gate.
    reader_pane: Option<u64>,
    /// Last command line seen while it was running, for the finish sentence.
    reader_cmd: String,
    /// OSC 133 mark already observed. A new absolute row is a new finish.
    reader_mark: Option<u64>,
    /// The mark present when a reader first looked is not announced.
    reader_mark_ready: bool,
    /// Last live-region name. Unchanged text does not raise another event.
    reader_live: Option<String>,
    /// Resolved `-p` command line for that same tab. `None` keeps the
    /// configured shell. Ignored when `start_command` is set.
    start_profile_command: Option<String>,
    /// argv after `--`. When set, that program is the tab's process.
    start_command: Option<Vec<String>>,
    /// This process token is elevated. Session restore and `config.shell`
    /// are not consulted: both are files a standard user can write.
    elevated: bool,
    /// Replaces the window content rect in tests, so `locate` and the
    /// wakeup path can run without a real window.
    #[cfg(test)]
    test_content: Option<render::Rect>,
    /// Watches config.json. A panel save tells it to ignore that write.
    config_watch: config_watch::ConfigWatcher,
    /// Which config the in-flight renderer build captured, and which one
    /// the session wants. A reload during a build is finished when that
    /// build lands.
    config_build: config_watch::RendererSlot,
    /// Parse error, or the list of settings that wait for a new pane.
    config_notice: Option<ConfigNotice>,
}

/// What a reload does to the session. The watcher's plan is not kept:
/// `plan` is `diff(live, merged)`.
struct ReloadDecision {
    config: config::UserConfig,
    load_error: Option<String>,
    plan: config_watch::ApplyPlan,
    deferred: Vec<config_watch::DeferredSetting>,
    /// Notice when the file overrode a value the open panel had edited.
    replaced: Option<String>,
}

fn decide_reload(
    live: &config::UserConfig,
    panel_open: bool,
    msg: config_watch::ReloadMsg,
) -> ReloadDecision {
    match msg {
        config_watch::ReloadMsg::Rejected { error } => ReloadDecision {
            config: live.clone(),
            load_error: Some(error),
            plan: config_watch::ApplyPlan::default(),
            deferred: Vec::new(),
            replaced: None,
        },
        config_watch::ReloadMsg::Applied { config, base, .. } => {
            // File fields that moved since `base` win. Everything else stays
            // as the panel left it. The plan is then live versus that merge,
            // not live versus the raw file and not the watcher's disk diff.
            let (merged, overrode) = config_watch::merge_reload(&base, live, &config);
            let (plan, deferred) = plan_against_live(live, &merged);
            let replaced = open_panel_file_notice(panel_open && overrode, live, &merged);
            ReloadDecision {
                config: merged,
                load_error: None,
                plan,
                deferred,
                replaced,
            }
        }
    }
}

/// Bytes to write, or the load error that blocks the save. A missing file
/// is not a load error: the caller can recreate config.json.
fn save_bytes(load_error: &Option<String>, config: &config::UserConfig) -> Result<Vec<u8>, String> {
    if let Some(error) = load_error {
        return Err(error.clone());
    }
    config::encode(config).map_err(|error| error.to_string())
}

/// How an external reload meets an open settings panel.
///
/// `plan_against_live` is `diff(live, merged)`. It must stay that
/// comparison: diffing the raw file against itself, or the live config
/// against the raw file, either rebuilds a font the merge kept or skips
/// a font the file actually changed.
fn plan_against_live(
    live: &config::UserConfig,
    incoming: &config::UserConfig,
) -> (config_watch::ApplyPlan, Vec<config_watch::DeferredSetting>) {
    config_watch::diff(live, incoming)
}

/// Set when the open panel had edited a field and the file changed that
/// same field to something else. The file's value is the one kept.
fn open_panel_file_notice(
    panel_open: bool,
    live: &config::UserConfig,
    incoming: &config::UserConfig,
) -> Option<String> {
    if panel_open && live != incoming {
        Some(
            tr(
                "تغيّر ملف الإعدادات قيمة عدّلتها اللوحة، فصارت قيمة الملف هي المعتمدة.",
                "The settings file changed a value the panel had edited, so the file's value is now in effect.",
            )
            .to_string(),
        )
    } else {
        None
    }
}

fn reload_notice(
    replaced: Option<String>,
    deferred: &[config_watch::DeferredSetting],
) -> Option<ConfigNotice> {
    let later = config_watch::deferred_notice(deferred);
    let text = match (replaced, later) {
        (Some(replaced), Some(later)) => format!("{replaced} {later}"),
        (Some(text), None) | (None, Some(text)) => text,
        (None, None) => return None,
    };
    Some(ConfigNotice {
        text,
        until: Some(Instant::now() + std::time::Duration::from_secs(8)),
    })
}

impl App {
    /// The only `App { ... }` in the crate. `run` and the tests both call
    /// this, so a new field is added here and nowhere else. A test that
    /// needs a different size or id counter changes that field afterwards.
    /// It must not write its own struct literal: another branch's new field
    /// would then be missing, and the tests would fail to build with no
    /// textual conflict.
    pub(crate) fn new(
        proxy: EventLoopProxy<UserEvent>,
        config: config::UserConfig,
        config_load_error: Option<String>,
        start_dir: Option<String>,
    ) -> Self {
        let keymap = keybinds::effective_map(&config);
        // no thread and no file: `run` swaps in the real watcher, and a
        // test never watches `~/.bayan`
        let config_watch = config_watch::ConfigWatcher::idle(config.clone());
        let config_build = config_watch::RendererSlot::new(config.clone());
        Self {
            posh: None,
            last_toast: None,
            mru: mru::Mru::new(),
            groups: groups::Groups::new(),
            spawn_group: None,
            palette: None,
            shell_arrow_focused: false,
            shell_profiles: Vec::new(),
            shell_profiles_ready: false,
            startup_pending: true,
            last_strip_click: None,
            proxy,
            window: None,
            gpu: None,
            renderer: None,
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            size: TermSize {
                cols: 110,
                rows: 32,
            },
            modifiers: ModifiersState::default(),
            first_frame: false,
            first_output: false,
            last_frame: Vec::new(),
            last_frame_size: None,
            cursor_pos: PhysicalPosition::new(0.0, 0.0),
            ime_spot: None,
            ime_area_reset: false,
            mouse: mouse::Route::default(),
            sel_pane: 0,
            drag_divider: None,
            bars: scrollbar::Bars::default(),
            cockpit: false,
            cockpit_sel: 0,
            pending_paste: None,
            pending_close: None,
            swallow_guard_release: false,
            link_hover: None,
            blocked_link: None,
            link_cache: link::SpanCache::default(),
            last_input: Instant::now(),
            blink_drawn_on: true,
            settings: None,
            focused: false,
            clicks: ClickTracker::new(),
            wheel_accum: 0.0,
            smooth: std::collections::HashMap::new(),
            smooth_at: None,
            alt_seen: std::collections::HashMap::new(),
            search: None,
            clipboard: None,
            auto_follow: true,
            claude_manual: false,
            image_budget: Arc::new(images::ImageBudget::new(config.images.limits())),
            json_config: config.clone(),
            lua_plugins: lua_host::PluginSet::default(),
            lua_pending: Vec::new(),
            lua_skipped: Vec::new(),
            lua_send_queue: std::collections::VecDeque::new(),
            lua_errors: Vec::new(),
            lua_notice: None,
            lua_approve_not_before: None,
            config,
            config_load_error,
            startup_error: None,
            keymap,
            shortcuts: None,
            atlas_generation: 0,
            font_delta: 0.0,
            renderer_building: false,
            normal_geom: None,
            keys_held_away: Vec::new(),
            debug_paste: None,
            start_dir,
            start_profile_command: None,
            start_command: None,
            elevated: false,
            #[cfg(test)]
            test_content: None,
            config_watch,
            config_build,
            config_notice: None,
            mux_client: None,
            a11y: a11y::A11ySnapshot::neutral(),
            inflight_a11y: a11y::A11ySnapshot::neutral(),
            reader: None,
            reader_seen_client: false,
            reader_gen: 0,
            reader_pace: screen_reader::TreePace::default(),
            reader_tree_at: None,
            reader_origin: Instant::now(),
            reader_deadline: None,
            reader_gate: screen_reader::OutputGate::default(),
            reader_bodies: Vec::new(),
            reader_pane: None,
            reader_cmd: String::new(),
            reader_mark: None,
            reader_mark_ready: false,
            reader_live: None,
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Re-read High Contrast and the text-size slider. A real change
    /// rebuilds the renderer so chrome colors and font metrics update
    /// without a restart. An unrelated `WM_SETTINGCHANGE` (a wallpaper,
    /// a policy) that leaves both values alone does not rebuild.
    fn reload_a11y(&mut self) {
        let next = a11y::read_snapshot();
        if next == self.a11y {
            return;
        }
        self.a11y = next;
        self.rebuild_renderer();
        self.request_redraw();
    }

    /// (Re)build the renderer off-thread for the current scale, config and
    /// zoom. One build runs at a time. A newer config that arrives while
    /// it runs is kept by `config_build` and started from `RendererReady`.
    fn rebuild_renderer(&mut self) {
        scroll::apply_font_change(&mut self.smooth);
        self.config_build.note_wanted(&self.config);
        if self.renderer_building {
            return;
        }
        let Some(w) = &self.window else { return };
        if !self.config_build.try_begin(self.font_delta) {
            return;
        }
        self.renderer_building = true;
        let proxy = self.proxy.clone();
        let scale = w.scale_factor() as f32;
        let cfg = self.config_build.snapshot().clone();
        let delta = self.font_delta;
        let snap = self.a11y;
        self.inflight_a11y = snap;
        std::thread::spawn(move || {
            let r = render::Renderer::with_a11y(scale, &cfg, delta, snap);
            let _ = proxy.send_event(UserEvent::RendererReady(Box::new(r)));
        });
    }

    // ---- settings panel ----

    /// Index of the active named theme in the gallery order (built-ins, then
    /// plugin themes). `None` while the user is on system/dark/light, so a
    /// mode word does not light a tile it does not name.
    fn active_theme(&self) -> Option<usize> {
        let name = self.config.theme.as_deref()?;
        if let Some(i) = render::THEMES.iter().position(|t| t.name == name) {
            return Some(i);
        }
        plugins::registry()
            .themes
            .iter()
            .position(|t| t.name == name)
            .map(|i| render::THEMES.len() + i)
    }

    /// Apply theme `idx` (copy its colors into the config), live.
    fn apply_theme(&mut self, idx: usize) {
        // one index into one list: built-ins first, then plugin themes. The
        // gallery draws from the same order, so tile N and theme N agree.
        let all = render::gallery();
        if let Some(t) = all.get(idx) {
            let builtin = render::THEMES.len();
            let name = if idx < builtin {
                render::THEMES[idx].name.to_string()
            } else {
                plugins::registry().themes[idx - builtin].name.clone()
            };
            self.remember(name, |c, v| c.theme = Some(v));
            self.remember(hex(t.bg), |c, v| c.bg = Some(v));
            self.remember(hex(t.fg), |c, v| c.fg = Some(v));
            self.remember(
                t.palette.iter().map(|&c| hex(c)).collect::<Vec<_>>(),
                |c, v| c.palette = Some(v),
            );
            self.push_colors();
        }
    }

    /// النظام / داكن / فاتح. Clears a named scheme and hand-set colours so
    /// the pair can show; that clear is the user's new choice.
    fn set_appearance(&mut self, choice: theme::ThemeChoice) {
        let theme = match choice {
            theme::ThemeChoice::System => None,
            theme::ThemeChoice::Dark => Some("dark".to_string()),
            theme::ThemeChoice::Light => Some("light".to_string()),
        };
        self.remember(theme, |c, v| {
            c.bg = None;
            c.fg = None;
            c.palette = None;
            c.theme = v;
        });
        self.push_colors();
    }

    /// Paint the colours `resolved_colors` names, and match the caption to them.
    fn push_colors(&mut self) {
        let colors = render::resolved_colors(&self.config);
        if let Some(r) = self.renderer.as_mut() {
            r.set_colors(colors.bg, colors.fg, colors.palette);
            // OSC 10/11 answer with what is on screen now
            term::set_query_colors(r.query_colors());
        }
        self.set_title_bar_dark(colors.title_bar_dark);
        self.request_redraw();
    }

    fn theme_surface(&self) -> theme::ThemeSurface {
        theme::ThemeSurface {
            colors: render::resolved_colors(&self.config),
            follows_system: render::follows_system_theme(&self.config),
        }
    }

    /// A Windows theme signal. The tested `apply_event` decides the palette
    /// and the caption flag; an explicit scheme comes back unchanged.
    fn on_system_theme(&mut self, event: theme::SystemThemeEvent) {
        let after = self.theme_surface().apply_event(event);
        // a runtime sample: both documents carry it, neither writes it
        self.remember(Some(event.is_light()), |c, v| c.system_apps_light = v);
        if let Some(r) = self.renderer.as_mut() {
            r.set_colors(after.colors.bg, after.colors.fg, after.colors.palette);
            // a program that asks OSC 11 after the switch sees the new side
            term::set_query_colors(r.query_colors());
        }
        self.set_title_bar_dark(after.colors.title_bar_dark);
        self.request_redraw();
    }

    fn change_font_size(&mut self, delta: i32) {
        let s = (self.config.font_size.unwrap_or(render::DEFAULT_FONT_SIZE) + delta as f32)
            .clamp(8.0, 40.0);
        self.remember(s, |c, v| c.font_size = Some(v));
        self.rebuild_renderer();
        self.request_redraw();
    }

    fn toggle_ligatures(&mut self) {
        let v = !self.config.ligatures.unwrap_or(true);
        self.remember(v, |c, v| c.ligatures = Some(v));
        self.rebuild_renderer();
        self.request_redraw();
    }

    /// Step to the previous/next installed monospace family. The renderer
    /// rebuild applies it live (same path as the theme).
    fn cycle_font(&mut self, dir: i32) {
        let Some(r) = self.renderer.as_ref() else {
            return;
        };
        let choices = r.font_choices();
        if choices.len() < 2 {
            return;
        }
        let cur = r.family();
        let i = choices.iter().position(|c| c == cur).unwrap_or(0);
        let next = choices[keyroute::cycle(i, dir, choices.len())].clone();
        self.remember(next, |c, v| c.font_family = Some(v));
        self.rebuild_renderer();
        self.request_redraw();
    }

    /// Step through Auto, then every installed font that draws Arabic.
    fn cycle_arabic_font(&mut self, dir: i32) {
        let Some(r) = self.renderer.as_ref() else {
            return;
        };
        let choices = r.arabic_font_choices();
        let current = r.arabic_family().map(str::to_string);
        let next = step_arabic_font(&choices, current.as_deref(), dir);
        self.remember(next, |c, v| c.arabic_font = v);
        self.rebuild_renderer();
        self.request_redraw();
    }

    fn set_cursor_style(&mut self, s: config::CursorStyle) {
        self.remember(s.as_str().to_string(), |c, v| c.cursor_style = Some(v));
        self.request_redraw(); // read per-frame; no rebuild needed
    }

    fn toggle_blink(&mut self) {
        let on = !self.config.cursor_blink_on();
        self.remember(on, |c, v| c.cursor_blink = Some(v));
        self.last_input = Instant::now(); // preview the blink right away
        self.request_redraw();
    }

    /// Walk the scrollback presets. Applies to new tabs only (the universal
    /// terminal convention — live sessions keep their buffer).
    fn change_scrollback(&mut self, dir: i32) {
        let cur = self.config.scrollback_lines();
        let n = step_preset(&config::SCROLLBACK_STEPS, cur, dir) as u32;
        self.remember(n, |c, v| c.scrollback = Some(v));
        self.request_redraw();
    }

    fn toggle_copy_on_select(&mut self) {
        let on = !self.config.copy_on_select_on();
        self.remember(on, |c, v| c.copy_on_select = Some(v));
        self.request_redraw();
    }

    fn set_bell(&mut self, mode: config::BellMode) {
        self.remember(mode.as_str().to_string(), |c, v| c.bell = Some(v));
        if mode == config::BellMode::Sound {
            beep(); // preview the mode you just picked
        }
        self.request_redraw();
    }

    /// Step to the previous/next installed shell. New tabs only.
    fn cycle_shell(&mut self, dir: i32) {
        let choices = &self.shell_profiles;
        if choices.len() < 2 {
            return;
        }
        let cur = self.config.shell_program();
        let i = shells::index_of(choices, &cur).unwrap_or(0);
        let command = choices[keyroute::cycle(i, dir, choices.len())]
            .command
            .clone();
        self.remember(command, |c, v| c.shell = Some(v));
        self.request_redraw();
    }

    fn change_padding(&mut self, dir: i32) {
        let cur = self.config.padding_px() as u32;
        let n = step_preset(&config::PADDING_STEPS, cur, dir);
        self.remember(n, |c, v| c.padding = Some(v));
        self.relayout_active(); // pane rects (and PTY sizes) follow the inset
        self.request_redraw();
    }

    fn change_opacity(&mut self, dir: i32) {
        let cur = (self.config.opacity_level() * 100.0).round() as u32;
        let n = step_preset(&config::OPACITY_STEPS, cur, dir) as f32 / 100.0;
        self.remember(n, |c, v| c.opacity = Some(v));
        self.apply_opacity();
        self.request_redraw();
    }

    /// Window opacity via the layered-window alpha (whole window, text
    /// included — per-pixel background alpha needs a transparent swapchain,
    /// which DX12 flip-model doesn't offer wgpu today).
    /// The window's Win32 handle (tray icon, toasts, layered alpha).
    fn hwnd(&self) -> Option<windows_sys::Win32::Foundation::HWND> {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let w = self.window.as_ref()?;
        let RawWindowHandle::Win32(h) = w.window_handle().ok()?.as_raw() else {
            return None;
        };
        Some(h.hwnd.get() as _)
    }

    /// Remove the tray icon, persist the session, leave. Every exit path
    /// funnels here so no ghost icon lingers in the tray.
    fn quit(&mut self, el: &ActiveEventLoop) {
        if let Some(h) = self.hwnd() {
            toast::remove(h);
        }
        // tabs + cwds greet you tomorrow. An elevated window does not write
        // session.json: that file is the next launch's command list.
        if elevate::may_write_bayan(self.elevated) && !self.save_state() {
            // The window is about to close, so its error card cannot be seen.
            // An explicit save failure must be acknowledged before exit.
            if let Some(error) = &self.blocked_link {
                show_error_dialog(self.hwnd(), &error.reason, &error.shown);
            }
        }
        // BAYAN-014: end every shell (and, through ConPTY, its console) now,
        // not whenever the process teardown gets to it
        for t in &mut self.tabs {
            for p in &mut t.panes {
                p.session.kill();
            }
        }
        el.exit();
    }

    fn resolve_close(&mut self, confirm: bool, el: &ActiveEventLoop) {
        if let Some(target) = self.pending_close.take().filter(|_| confirm) {
            match target {
                CloseTarget::Pane(ti, pi) => self.remove_pane(ti, pi, el),
                CloseTarget::Tab(ti) => self.remove_tab(ti, el),
                CloseTarget::Window => self.quit(el),
            }
        }
        self.request_redraw();
    }

    fn apply_opacity(&self) {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Some(w) = &self.window else { return };
        let Ok(handle) = w.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return;
        };
        let hwnd = h.hwnd.get() as windows_sys::Win32::Foundation::HWND;
        let alpha = (self.config.opacity_level() * 255.0).round() as u8;
        // SAFETY: hwnd comes from winit's handle for self.window, which stays
        // borrowed for this whole call, so it names our live top-level window.
        // The GWL_EXSTYLE get/set and SetLayeredWindowAttributes take no pointers.
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE,
                LWA_ALPHA, WS_EX_LAYERED,
            };
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            if alpha == 255 {
                // fully opaque: drop the layered style (no compositing tax)
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED as isize));
            } else {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
                SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA);
            }
        }
    }

    fn apply_window_frame(&self) {
        self.set_title_bar_dark(render::resolved_colors(&self.config).title_bar_dark);
    }

    fn set_title_bar_dark(&self, title_bar_dark: bool) {
        let Some(hwnd) = self.hwnd() else { return };
        use windows_sys::Win32::Graphics::Dwm::{
            DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
            DWMWCP_ROUND,
        };
        let dark = i32::from(title_bar_dark);
        let corner = DWMWCP_ROUND;
        // Windows 10 ignores unsupported corner preferences; the native
        // resize frame and shadow remain available there.
        // SAFETY: hwnd is this window's live handle from self.hwnd(). Each
        // pvAttribute points at a local i32 that outlives the call, and
        // cbAttribute is that i32's size, the type both attributes expect.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                (&dark as *const i32).cast(),
                std::mem::size_of_val(&dark) as u32,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                (&corner as *const i32).cast(),
                std::mem::size_of_val(&corner) as u32,
            );
        }
    }

    fn toggle_hide_bar(&mut self) {
        let on = !self.config.hide_single_tab_on();
        self.remember(on, |c, v| c.hide_single_tab = Some(v));
        self.relayout_active(); // the content area gains/loses the bar strip
        self.request_redraw();
    }

    fn toggle_confirm_close(&mut self) {
        let on = !self.config.confirm_close_on();
        self.remember(on, |c, v| c.confirm_close = Some(v));
        self.request_redraw();
    }

    fn toggle_notifications(&mut self) {
        let on = !self.config.notifications_on();
        self.remember(on, |c, v| c.notifications = Some(v));
        if on {
            // preview the toast you just turned on
            if let Some(h) = self.hwnd() {
                toast::show(
                    h,
                    tr("الإشعارات مفعّلة", "Notifications on"),
                    tr(
                        "هكذا يبدو إشعار اكتمال الأمر",
                        "This is how a finished-command notification looks",
                    ),
                );
            }
        }
        self.request_redraw();
    }

    /// Open the prompt picker. Each preview is one oh-my-posh subprocess (up
    /// to 3 s), so they render on a `bayan-posh` worker and stream in through
    /// `UserEvent::PoshPreview` (BAYAN-006). Only the featured themes and the
    /// selected one render on open (R-04): rendering a theme runs it, and a
    /// downloaded pack is ~100 files nobody asked to run. The rest render
    /// when highlighted. A burst keeps the newest highlight and reports the
    /// ones it skipped, so coming back asks again. Closing or reopening
    /// stops the worker.
    fn open_posh(&mut self) {
        if self.elevated {
            let (title, body) = elevate::prompt_picker_notice();
            self.blocked_link = Some(BlockedLink {
                reason: title.to_string(),
                shown: body.to_string(),
                confirm: None,
                is_app_error: true,
            });
            self.request_redraw();
            return;
        }
        use std::sync::atomic::Ordering;
        let names = posh::available();
        if names.is_empty() {
            return; // no oh-my-posh themes: nothing to pick from
        }
        let cur = posh::current();
        let sel = cur
            .and_then(|c| names.iter().position(|n| *n == c))
            .unwrap_or(0);
        let generation = POSH_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        // (row, theme, eager). Eager rows all render; highlight requests
        // collapse to the newest, so holding an arrow key does not queue a
        // subprocess per row passed over. Skipped highlights come back as
        // PoshDropped and their claims are cleared.
        let (tx, rx) = std::sync::mpsc::channel::<(usize, String, bool)>();
        let proxy = self.proxy.clone();
        let _ = std::thread::Builder::new()
            .name("bayan-posh".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    let mut burst = vec![first];
                    while let Ok(next) = rx.try_recv() {
                        burst.push(next);
                    }
                    let (run, dropped) = collapse_posh_requests(burst);
                    if POSH_GENERATION.load(Ordering::SeqCst) != generation {
                        return; // the picker closed or reopened
                    }
                    if !dropped.is_empty()
                        && proxy
                            .send_event(UserEvent::PoshDropped(generation, dropped))
                            .is_err()
                    {
                        return; // the event loop is gone
                    }
                    for (i, name, _) in run {
                        if POSH_GENERATION.load(Ordering::SeqCst) != generation {
                            return; // the picker closed or reopened
                        }
                        let preview = posh::preview(&name);
                        if proxy
                            .send_event(UserEvent::PoshPreview(generation, i, preview))
                            .is_err()
                        {
                            return; // the event loop is gone
                        }
                    }
                }
            });
        let mut asked = vec![false; names.len()];
        for i in posh::eager_previews(&names, sel) {
            asked[i] = true;
            let _ = tx.send((i, names[i].clone(), true));
        }
        self.posh = Some(PoshPicker {
            previews: vec![None; names.len()],
            asked,
            names,
            sel,
            generation,
            work: tx,
        });
        self.request_redraw();
    }

    /// Ask the worker for the highlighted theme's preview, once.
    fn request_posh_preview(&mut self) {
        let Some(p) = self.posh.as_mut() else { return };
        let i = p.sel;
        let have = p.previews.get(i).is_some_and(Option::is_some);
        if !claim_posh_preview(&mut p.asked, have, i) {
            return;
        }
        let _ = p.work.send((i, p.names[i].clone(), false));
    }

    /// Close the picker and stop its preview worker.
    fn close_posh(&mut self) {
        POSH_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.posh = None;
    }

    fn posh_input(&mut self, event: &KeyEvent) {
        use winit::keyboard::{Key, NamedKey};
        let Some(p) = self.posh.as_mut() else { return };
        let n = p.names.len();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.close_posh(),
            Key::Named(NamedKey::ArrowDown) => {
                p.sel = keyroute::cycle(p.sel, 1, n);
                self.request_posh_preview();
            }
            Key::Named(NamedKey::ArrowUp) => {
                p.sel = keyroute::cycle(p.sel, -1, n);
                self.request_posh_preview();
            }
            Key::Named(NamedKey::Enter) => {
                let name = p.names[p.sel].clone();
                self.close_posh();
                if self.elevated {
                    let (title, body) = elevate::prompt_picker_notice();
                    self.blocked_link = Some(BlockedLink {
                        reason: title.to_string(),
                        shown: body.to_string(),
                        confirm: None,
                        is_app_error: true,
                    });
                } else {
                    match posh::set_theme(&name) {
                        Ok(()) => eprintln!("bayan: prompt theme set to {name} (new tabs)"),
                        Err(e) => eprintln!("bayan: prompt theme failed: {e}"),
                    }
                }
            }
            _ => {}
        }
        self.request_redraw();
    }

    fn open_palette(&mut self, query: String) {
        self.abandon_mouse();
        let links = self
            .session()
            .map(|session| {
                let t = session.term.lock();
                (0..t.screen_lines())
                    .flat_map(|row| link::links_on_row(&t, row))
                    .take(500)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut extras = palette_extras(&self.shell_profiles, &links);
        // An elevated window does not list SSH or the mux. The same bit
        // `elevate::process_is_elevated` stored at startup.
        if !self.elevated {
            extras.push((PaletteRow::OpenSsh, ssh::label_open().to_string()));
            if self.config.mux_enabled() {
                extras.push((PaletteRow::DetachSession, mux::label_detach().to_string()));
                extras.push((PaletteRow::OpenDetached, mux::label_reattach().to_string()));
            }
        }
        self.palette = Some(Palette {
            query,
            rename: None,
            menu: None,
            shells_menu: false,
            ssh_menu: false,
            detached_menu: false,
            shell_arrow: false,
            sel: 0,
            extras,
            pressed: None,
            wheel: 0.0,
        });
        self.request_redraw();
    }

    /// The palette owns the keyboard while open: type to filter, arrows to
    /// move, Enter to run, Esc to dismiss.
    fn palette_input(&mut self, event: &KeyEvent, el: &ActiveEventLoop) {
        use winit::keyboard::{Key, NamedKey};
        if let Some(p) = self.palette.as_mut() {
            p.pressed = None;
            p.wheel = 0.0;
        }
        if self.modifiers.control_key()
            && !self.modifiers.alt_key()
            && !self.palette.as_ref().is_some_and(|p| p.shell_arrow)
            && event.physical_key
                == winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyV)
        {
            if let Some(text) = self.clipboard().and_then(|c| c.get_text().ok()) {
                if let Some(p) = self.palette.as_mut() {
                    p.query
                        .extend(text.chars().filter(|c| !c.is_control()).take(256));
                    p.sel = 0;
                }
            }
            self.request_redraw();
            return;
        }
        let arrow_menu = self.palette.as_ref().is_some_and(|p| p.shell_arrow);
        let Some(p) = self.palette.as_mut() else {
            return;
        };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.palette = None;
                if arrow_menu {
                    self.shell_arrow_focused = true;
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if !p.shell_arrow {
                    p.query.pop();
                    p.sel = 0;
                }
            }
            Key::Named(
                k @ (NamedKey::ArrowDown
                | NamedKey::ArrowUp
                | NamedKey::ArrowLeft
                | NamedKey::ArrowRight),
            ) => {
                let shells = self.palette.as_ref().is_some_and(|p| p.shells_menu);
                let step = if shells {
                    let menu_key = match k {
                        NamedKey::ArrowUp => shellmenu::MenuKey::Up,
                        NamedKey::ArrowLeft => shellmenu::MenuKey::Left,
                        NamedKey::ArrowRight => shellmenu::MenuKey::Right,
                        _ => shellmenu::MenuKey::Down,
                    };
                    match shellmenu::menu_action(menu_key, lang::rtl()) {
                        shellmenu::MenuAction::Move(step) => step,
                        _ => 0,
                    }
                } else if matches!(k, NamedKey::ArrowUp | NamedKey::ArrowDown) {
                    if *k == NamedKey::ArrowUp {
                        -1
                    } else {
                        1
                    }
                } else {
                    0
                };
                if step != 0 {
                    let n = self
                        .palette_rows(&self.palette.as_ref().unwrap().query.clone())
                        .len();
                    if n > 0 {
                        let p = self.palette.as_mut().unwrap();
                        p.sel = keyroute::cycle(p.sel, step, n);
                    }
                }
            }
            Key::Named(NamedKey::Enter) => {
                let q = p.query.clone();
                let sel = p.sel;
                if let Some((row, _)) = self.palette_rows(&q).into_iter().nth(sel) {
                    self.palette_run(row, el);
                    return;
                }
                self.palette = None;
            }
            Key::Character(c) => {
                // The arrow's menu is a menu, not a search field.
                if !p.shell_arrow {
                    p.query.push_str(c);
                    p.sel = 0; // a new filter invalidates the old position
                }
            }
            // winit names the space bar; without this a label with a space
            // in it (most Arabic labels, every tab name of two words) could
            // not be typed
            Key::Named(NamedKey::Space) => {
                if !p.shell_arrow {
                    p.query.push(' ');
                    p.sel = 0;
                }
            }
            _ => {}
        }
        self.request_redraw();
    }

    /// Palette rows matching the query, built-ins first then plugin actions.
    /// Matching is case-insensitive and substring, on the label the user
    /// actually sees — searching a hidden id would be a guessing game.
    fn palette_rows(&self, query: &str) -> Vec<(PaletteRow, String)> {
        if let Some(target) = self.palette.as_ref().and_then(|p| p.rename) {
            return rename_rows(target, query);
        }
        if self
            .palette
            .as_ref()
            .is_some_and(|p| p.ssh_menu || p.detached_menu)
        {
            let q = query.trim().to_lowercase();
            return self
                .palette
                .as_ref()
                .unwrap()
                .extras
                .iter()
                .filter(|(_, label)| q.is_empty() || label.to_lowercase().contains(&q))
                .cloned()
                .collect();
        }
        if self.palette.as_ref().is_some_and(|p| p.menu.is_some()) {
            let q = query.trim().to_lowercase();
            let rows = if self.palette.as_ref().is_some_and(|p| p.shells_menu) {
                self.shell_picker_rows()
                    .into_iter()
                    .map(|row| {
                        let action = match row.kind {
                            shellmenu::Kind::Shell => PaletteRow::Shell(row.command),
                            shellmenu::Kind::Ssh => PaletteRow::OpenSsh,
                            shellmenu::Kind::Mux => PaletteRow::OpenDetached,
                        };
                        (action, row.label)
                    })
                    .collect()
            } else {
                self.context_rows()
            };
            return rows
                .into_iter()
                .filter(|(_, label)| q.is_empty() || label.to_lowercase().contains(&q))
                .collect();
        }
        let q = query.trim().to_lowercase();
        let hit = |label: &str| q.is_empty() || label.to_lowercase().contains(&q);
        let mut rows: Vec<(PaletteRow, String)> = Vec::new();
        let bidi_now = self.focused_bidi_mode();
        for a in keybinds::Action::ALL {
            let label = if a == keybinds::Action::BidiToggle {
                bidi_now.palette_label()
            } else {
                a.label().to_string()
            };
            // The live row names the current mode. The static label still
            // matches, so a search for the shortcut's name finds it.
            if hit(&label) || (a == keybinds::Action::BidiToggle && hit(a.label())) {
                rows.push((PaletteRow::Builtin(a), label));
            }
        }
        let group_labels: Vec<(u32, String)> = self
            .groups
            .iter()
            .map(|g| (g.id, self.groups.label(g.id)))
            .collect();
        rows.extend(
            group_rows(&group_labels, self.active_group())
                .into_iter()
                .filter(|(_, label)| hit(label)),
        );
        for (i, act) in plugins::registry().actions.iter().enumerate() {
            if hit(&plugin_row_label(&act.label)) {
                // BAYAN-007: badged, so a plugin row never reads as built-in
                rows.push((PaletteRow::Plugin(i), plugin_row_label(&act.label)));
            }
        }
        if let Some(palette) = &self.palette {
            rows.extend(
                palette
                    .extras
                    .iter()
                    .filter(|(_, label)| hit(label))
                    .cloned(),
            );
        }
        rows
    }

    fn palette_run(&mut self, row: PaletteRow, el: &ActiveEventLoop) {
        self.palette = None;
        // A pick leaves the menu. Focus goes to the new tab, not the arrow.
        self.shell_arrow_focused = false;
        match row {
            // the SAME dispatch a keybinding uses, not a parallel copy
            PaletteRow::Builtin(a) => self.perform_action(a, el),
            PaletteRow::Shell(command) => {
                let cwd = self
                    .session()
                    .and_then(|s| s.meta.lock_or_recover().spawn_cwd());
                let before = self.tabs.len();
                self.spawn_tab_command(cwd, &command, None);
                if self.tabs.len() == before && self.blocked_link.is_none() {
                    self.blocked_link = Some(BlockedLink {
                        reason: tr("تعذر تشغيل الطرفية", "Could not start the shell").into(),
                        shown: link::preview(&command, 240),
                        confirm: None,
                        is_app_error: false,
                    });
                }
            }
            PaletteRow::CopyText(text) => self.set_clipboard(text),
            PaletteRow::RenameTab(name) => self.rename_tab(name),
            PaletteRow::ShowGroup(g) => self.show_group(g),
            PaletteRow::MoveToGroup(g) => self.move_tab_to_group(g),
            PaletteRow::OpenRenameGroup => self.open_rename_group(),
            PaletteRow::OpenShellMenu => self.open_shell_menu(),
            PaletteRow::RenameGroup(name) => self.rename_group(name),
            PaletteRow::OpenSsh => self.open_ssh_menu(),
            PaletteRow::SshHost(token) => self.open_ssh_host(&token),
            PaletteRow::DetachSession => self.detach_focused(el),
            PaletteRow::OpenDetached => self.open_detached_menu(),
            PaletteRow::Reattach(id) => self.reattach_session(id),
            PaletteRow::KillSession(id) => self.kill_detached(id),
            PaletteRow::InsertCommand(text) => {
                self.pending_paste = Some(PendingPaste {
                    kind: PendingKind::Paste,
                    pane: self.focused_pane_id(),
                    text: text.into(),
                    lines: 1,
                    chars: text.chars().count(),
                    reason: tr(
                        "إدراج أمر من لوحة الأوامر",
                        "Insert a command from the palette",
                    )
                    .into(),
                    lua: None,
                    scroll: 0,
                    saw_end: true,
                });
            }
            PaletteRow::Plugin(i) => {
                // BAYAN-007: never a raw PTY write. Like InsertCommand, the
                // text waits on the paste-guard card (Enter sends it through
                // send_paste → normalize_paste, Esc drops it), and the card
                // names the plugin action that asked.
                if let Some(act) = plugins::registry().actions.get(i) {
                    let pane = self.focused_pane_id();
                    self.pending_paste = Some(plugin_pending(&act.label, &act.send, pane));
                }
            }
        }
        self.request_redraw();
    }

    /// Run one rebindable action (the keymap's dispatch target).
    fn perform_action(&mut self, action: keybinds::Action, el: &ActiveEventLoop) {
        use keybinds::Action::*;
        match action {
            NewTab => {
                // new tab inherits the active pane's directory, and only if
                // that directory exists — a stale OSC 7 must not become the cwd
                let cwd = self
                    .session()
                    .and_then(|s| s.meta.lock_or_recover().spawn_cwd());
                self.spawn_tab(cwd);
                self.request_redraw();
            }
            ClosePane => {
                let (ti, pi) = (
                    self.active,
                    self.tabs.get(self.active).map_or(0, |t| t.focused),
                );
                if self.config.confirm_close_on() && self.running_command(ti, pi).is_some() {
                    self.pending_close = Some(CloseTarget::Pane(ti, pi));
                    self.request_redraw();
                    return;
                }
                self.remove_pane(ti, pi, el);
            }
            Search => {
                let selected = self
                    .session()
                    .and_then(|s| s.term.lock().selection_to_string());
                self.search = Some(SearchState::from_selection(
                    selected.as_deref(),
                    self.claude_active(),
                ));
                self.request_redraw();
            }
            Settings => {
                self.settings = Some(0);
                self.request_redraw();
            }
            Copy => {
                self.copy_selection(false);
            }
            Paste => self.paste(),
            SplitH => self.split(Orientation::Row),
            SplitV => self.split(Orientation::Column),
            Cockpit => {
                self.cockpit = true;
                self.cockpit_sel = self.active;
                self.request_redraw();
            }
            PromptPrev => self.jump_command(-1),
            PromptNext => self.jump_command(1),
            ClaudeToggle => {
                // auto -> manual flip -> auto (EasyTer's toggle semantics)
                if self.auto_follow {
                    self.claude_manual = !self.claude_active();
                    self.auto_follow = false;
                } else {
                    self.auto_follow = true;
                }
                self.request_redraw();
            }
            BidiToggle => self.toggle_bidi_reorder(),
            ResetKeyboard => {
                if let Some(s) = self.session_mut() {
                    s.reset_keyboard();
                }
            }
            RenameTab => self.open_rename_tab(),
            Broadcast => self.toggle_broadcast(),
            ZoomPane => self.toggle_zoom(),
            CopyLastOutput => self.copy_last_output(),
            NewGroup => self.new_group(),
            NextGroup => self.step_group(false),
            PrevGroup => self.step_group(true),
            CopyMode => self.toggle_copy_mode(),
            CopyClean => {
                self.copy_selection_clean();
            }
        }
    }

    /// Keyboard while the shortcuts editor is open: arrows select, Enter
    /// captures, Delete restores a default, Esc closes (or cancels a
    /// capture). A capture takes the next bindable chord.
    fn shortcuts_input(&mut self, event: &KeyEvent, el: &ActiveEventLoop) {
        let _ = el;
        let Some(st) = self.shortcuts.as_mut() else {
            return;
        };
        let n = keybinds::Action::ALL.len();
        if st.capturing {
            if matches!(event.logical_key, Key::Named(NamedKey::Escape)) {
                st.capturing = false;
                self.request_redraw();
                return;
            }
            let (ctrl, shift, alt) = (
                self.modifiers.control_key(),
                self.modifiers.shift_key(),
                self.modifiers.alt_key(),
            );
            let Some(chord) = keybinds::chord_from(event.physical_key, ctrl, shift, alt) else {
                return; // a bare modifier — keep waiting for the real key
            };
            if !chord.is_bindable() {
                st.flash = Some(
                    tr(
                        "الاختصار يحتاج Ctrl أو Alt أو مفتاح F",
                        "A shortcut needs Ctrl, Alt or an F key",
                    )
                    .to_string(),
                );
                st.capturing = false;
                self.request_redraw();
                return;
            }
            let action = keybinds::Action::ALL[st.sel];
            if let Some(owner) = keybinds::lookup(&self.keymap, chord) {
                if owner != action {
                    st.flash = Some(if lang::rtl() {
                        format!("«{}» يستخدم هذا الاختصار", owner.label())
                    } else {
                        format!("“{}” already uses this shortcut", owner.label())
                    });
                    st.capturing = false;
                    self.request_redraw();
                    return;
                }
            }
            st.capturing = false;
            st.flash = None;
            self.set_binding(action, chord);
            return;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.shortcuts = None;
                self.save_config();
                self.request_redraw();
            }
            Key::Named(NamedKey::ArrowUp) => {
                st.sel = keyroute::cycle(st.sel, -1, n);
                st.flash = None;
                self.request_redraw();
            }
            Key::Named(NamedKey::ArrowDown) => {
                st.sel = keyroute::cycle(st.sel, 1, n);
                st.flash = None;
                self.request_redraw();
            }
            Key::Named(NamedKey::Enter) => {
                st.capturing = true;
                st.flash = None;
                self.request_redraw();
            }
            Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) => {
                let action = keybinds::Action::ALL[st.sel];
                st.flash = None;
                self.clear_binding(action);
            }
            _ => {}
        }
    }

    /// Store a binding (pruned back out if it IS the default) and rebuild
    /// the live keymap.
    fn set_binding(&mut self, action: keybinds::Action, chord: keybinds::Chord) {
        let stored = if chord == action.default_chord() {
            None
        } else {
            Some(chord.to_config())
        };
        apply_shortcut(
            &mut self.json_config.keybinds,
            &mut self.config.keybinds,
            action.id(),
            stored,
        );
        self.keymap = keybinds::effective_map(&self.config);
        self.request_redraw();
    }

    fn clear_binding(&mut self, action: keybinds::Action) {
        apply_shortcut(
            &mut self.json_config.keybinds,
            &mut self.config.keybinds,
            action.id(),
            None,
        );
        self.keymap = keybinds::effective_map(&self.config);
        self.request_redraw();
    }

    /// A click while the shortcuts editor is open: a row selects and starts
    /// a capture; anywhere else closes (saving).
    fn shortcuts_click(&mut self, px: f64, py: f64) {
        let Some((fw, fh)) = self.window.as_ref().map(|w| {
            let s = w.inner_size();
            (s.width as usize, s.height as usize)
        }) else {
            return;
        };
        let n = keybinds::Action::ALL.len();
        let Some(lay) = self
            .renderer
            .as_ref()
            .map(|r| r.shortcuts_layout(fw, fh, n))
        else {
            return;
        };
        if !render::rect_hit(lay.card, px, py) {
            self.shortcuts = None;
            self.save_config();
            self.request_redraw();
            return;
        }
        if let Some(i) = lay.rows.iter().position(|&r| render::rect_hit(r, px, py)) {
            if let Some(st) = self.shortcuts.as_mut() {
                st.sel = i;
                st.capturing = true;
                st.flash = None;
                self.request_redraw();
            }
        }
    }

    /// Blink phase now + when it next flips. (true, None) = solid cursor,
    /// nothing to schedule: blink off, window unfocused, or 15s idle.
    fn cursor_blink_state(&self) -> (bool, Option<Instant>) {
        if !self.config.cursor_blink_on() || !self.focused {
            return (true, None);
        }
        let elapsed = self.last_input.elapsed();
        if elapsed >= BLINK_STOP {
            return (true, None);
        }
        let k = elapsed.as_millis() / BLINK_MS;
        let on = k.is_multiple_of(2);
        let next_flip =
            self.last_input + std::time::Duration::from_millis(((k + 1) * BLINK_MS) as u64);
        // the wake at the 15s mark restores the solid cursor
        let deadline = self.last_input + BLINK_STOP;
        (on, Some(next_flip.min(deadline)))
    }

    /// The focused pane's running command, if any (drives the close guard).
    fn running_command(&self, ti: usize, pi: usize) -> Option<String> {
        let pane = self.tabs.get(ti)?.panes.get(pi)?;
        let cmd = pane.session.meta.lock_or_recover().running_cmd.clone();
        (!cmd.is_empty()).then_some(cmd)
    }

    /// How many panes across all tabs are mid-command right now.
    fn running_count(&self) -> usize {
        self.tabs
            .iter()
            .flat_map(|t| &t.panes)
            .filter(|p| !p.session.meta.lock_or_recover().running_cmd.is_empty())
            .count()
    }

    /// The settings header's language button: Arabic ⇄ English, at once
    /// and saved. Every label is read from `lang` each frame, so the next
    /// frame is wholly in the new language; the settings panel stays open,
    /// mirrored, with the button now naming the way back.
    fn switch_language(&mut self) {
        let next = self.config.lang().other();
        self.remember(next.id().to_string(), |c, v| c.language = Some(v));
        lang::set(next);
        self.save_config();
        self.request_redraw();
    }

    fn report_error(&mut self, reason: &str, detail: &str) {
        eprintln!("bayan: {reason}: {detail}");
        self.blocked_link = Some(BlockedLink {
            reason: reason.to_string(),
            shown: link::preview_with(detail, 240, |c| c != '\n' && link::is_forbidden(c)),
            confirm: None,
            is_app_error: true,
        });
        self.request_redraw();
    }

    /// A panel edit lands in the runtime config and in the JSON document.
    /// Lua-only values stay on `config` and are not copied here.
    fn remember<T: Clone>(&mut self, value: T, write: impl Fn(&mut config::UserConfig, T)) {
        write(&mut self.config, value.clone());
        write(&mut self.json_config, value);
    }

    /// The Arabic admin card. A config-load error already on the card stays;
    /// the notice is appended instead of replacing it.
    fn apply_admin_notice(&mut self) {
        if !self.elevated {
            return;
        }
        let (title, body) = elevate::admin_notice();
        if let Some(block) = self.blocked_link.as_mut() {
            block.shown = elevate::with_admin_notice(Some(&block.shown), title, body);
        } else {
            self.blocked_link = Some(BlockedLink {
                reason: title.to_string(),
                shown: body.to_string(),
                confirm: None,
                is_app_error: true,
            });
        }
    }

    fn save_config(&mut self) {
        if !elevate::may_write_bayan(self.elevated) {
            self.report_error(
                tr("نافذة مسؤول", "Administrator window"),
                tr(
                    "لا يُحفظ الإعداد من نافذة المسؤول.",
                    "Settings are not saved from an administrator window.",
                ),
            );
            return;
        }
        let bytes = match save_bytes(&self.config_load_error, &self.json_config) {
            Ok(bytes) => bytes,
            Err(error) => {
                if self.config_load_error.is_some() {
                    self.report_config_load_error();
                } else {
                    self.report_error(tr("تعذر حفظ الإعدادات", "Could not save settings"), &error);
                }
                return;
            }
        };
        // Arm the watcher before the rename, so the event that rename
        // produces is recognized as ours and not loaded back.
        self.config_watch.note_self_write(&bytes);
        if let Err(error) = config::write_bytes(&bytes) {
            self.config_watch.finish_self_write(false);
            self.report_error(
                tr("تعذر حفظ الإعدادات", "Could not save settings"),
                &error.to_string(),
            );
        } else {
            self.config_watch.finish_self_write(true);
        }
    }

    /// Apply a config.json change that did not come from the settings panel.
    ///
    /// Fields the file changed since the last load take the file's value.
    /// Every other field keeps the live panel value. The plan is
    /// `diff(live, merged)` via [`decide_reload`]. A bad file leaves
    /// `self.config` as it was. Font and color changes rebuild the renderer;
    /// when that renderer arrives, `relayout_active` resizes every PTY.
    /// Padding alone resizes with the metrics already on screen.
    fn apply_config_reload(&mut self, msg: config_watch::ReloadMsg) {
        // The merge runs on the JSON document, where panel edits also land.
        let decision = decide_reload(&self.json_config, self.settings.is_some(), msg);
        if let Some(error) = decision.load_error {
            self.config_load_error = Some(error.clone());
            self.config_notice = Some(ConfigNotice {
                text: config_watch::parse_error_notice(&error),
                until: None,
            });
            self.request_redraw();
            return;
        }
        self.config_load_error = None;
        // The running config also holds config.lua's overrides. A field the
        // file changed takes the file's value there too; every other field
        // keeps what is on screen. No script runs on a reload: a new hash in
        // `lua_approvals` is honoured by the confirm card or the next launch.
        let (runtime, _) =
            config_watch::merge_reload(&self.json_config, &self.config, &decision.config);
        let (plan, deferred) = plan_against_live(&self.config, &runtime);
        let decision = ReloadDecision {
            plan,
            deferred,
            ..decision
        };
        self.json_config = decision.config.clone();
        let language = decision.plan.language && runtime.lang() != self.config.lang();
        // the bar's side and width decide the gutter, so the grid moves
        let bar_moved = runtime.scrollbar != self.config.scrollbar;
        self.config = runtime;
        // published once at startup; a reload has to publish it again
        term::set_osc52_read(self.config.osc52_read_enabled());
        if language {
            lang::set(self.config.lang());
        }
        if decision.plan.keybinds {
            self.keymap = keybinds::effective_map(&self.config);
        }
        if decision.plan.opacity {
            self.apply_opacity();
        }
        let rebuild = decision.plan.font || decision.plan.colors;
        if rebuild {
            self.rebuild_renderer();
        } else if decision.plan.padding || decision.plan.hide_bar || bar_moved {
            self.relayout_active();
        }
        self.config_notice = reload_notice(decision.replaced, &decision.deferred);
        self.request_redraw();
    }

    fn report_config_load_error(&mut self) {
        if let Some(error) = &self.config_load_error {
            let detail = format!(
                "{}\n{}",
                error,
                tr(
                    "أصلح ملف config.json. بيان يُبقي آخر إعدادات سليمة ويعيد التحميل عند إصلاح الملف.",
                    "Fix config.json. Bayan is keeping the last good settings and will reload when the file is valid.",
                )
            );
            self.report_error(
                tr("تعذّر تحميل الإعدادات", "Could not load settings"),
                &detail,
            );
        }
    }

    fn close_settings(&mut self) {
        self.settings = None;
        self.save_config(); // persist on close
        self.request_redraw();
    }

    /// A click at (px, py) while the settings panel is open: apply the
    /// control it hit, or close if the click was outside the card.
    fn settings_click(&mut self, px: f64, py: f64) {
        let Some((fw, fh)) = self.window.as_ref().map(|w| {
            let s = w.inner_size();
            (s.width as usize, s.height as usize)
        }) else {
            return;
        };
        let Some(lay) = self.renderer.as_ref().map(|r| r.settings_layout(fw, fh)) else {
            return;
        };
        if !render::rect_hit(lay.card, px, py) {
            self.close_settings();
            return;
        }
        if render::rect_hit(lay.lang_btn, px, py) {
            self.switch_language();
        } else if render::rect_hit(lay.shortcuts_btn, px, py) {
            // hand the stage to the shortcuts editor (settings saves first)
            self.close_settings();
            self.shortcuts = Some(ShortcutsState::default());
            self.request_redraw();
        } else if let Some(i) = lay
            .theme_tiles
            .iter()
            .position(|&t| render::rect_hit(t, px, py))
        {
            self.apply_theme(i);
        } else if render::rect_hit(lay.font_prev, px, py) {
            self.cycle_font(-1);
        } else if render::rect_hit(lay.font_next, px, py) {
            self.cycle_font(1);
        } else if render::rect_hit(lay.size_minus, px, py) {
            self.change_font_size(-1);
        } else if render::rect_hit(lay.size_plus, px, py) {
            self.change_font_size(1);
        } else if render::rect_hit(lay.arabic_prev, px, py) {
            self.cycle_arabic_font(-1);
        } else if render::rect_hit(lay.arabic_next, px, py) {
            self.cycle_arabic_font(1);
        } else if let Some(i) = lay
            .cursor_btns
            .iter()
            .position(|&b| render::rect_hit(b, px, py))
        {
            self.set_cursor_style(config::CursorStyle::ALL[i]);
        } else if render::rect_hit(lay.blink_toggle, px, py) {
            self.toggle_blink();
        } else if render::rect_hit(lay.scroll_minus, px, py) {
            self.change_scrollback(-1);
        } else if render::rect_hit(lay.scroll_plus, px, py) {
            self.change_scrollback(1);
        } else if render::rect_hit(lay.copy_toggle, px, py) {
            self.toggle_copy_on_select();
        } else if render::rect_hit(lay.liga_toggle, px, py) {
            self.toggle_ligatures();
        } else if let Some(i) = lay
            .bell_btns
            .iter()
            .position(|&b| render::rect_hit(b, px, py))
        {
            self.set_bell(render::BELL_SEGMENTS[i]);
        } else if render::rect_hit(lay.notif_toggle, px, py) {
            self.toggle_notifications();
        } else if render::rect_hit(lay.pad_minus, px, py) {
            self.change_padding(-1);
        } else if render::rect_hit(lay.pad_plus, px, py) {
            self.change_padding(1);
        } else if render::rect_hit(lay.opacity_minus, px, py) {
            self.change_opacity(-1);
        } else if render::rect_hit(lay.opacity_plus, px, py) {
            self.change_opacity(1);
        } else if render::rect_hit(lay.shell_prev, px, py) {
            self.cycle_shell(-1);
        } else if render::rect_hit(lay.shell_next, px, py) {
            self.cycle_shell(1);
        } else if render::rect_hit(lay.bar_toggle, px, py) {
            self.toggle_hide_bar();
        } else if render::rect_hit(lay.close_toggle, px, py) {
            self.toggle_confirm_close();
        } else if let Some(i) = lay
            .appearance_btns
            .iter()
            .position(|&b| render::rect_hit(b, px, py))
        {
            self.set_appearance(theme::APPEARANCE_SEGMENTS[i]);
        }
    }

    /// Keyboard while the settings panel is open: Esc/Enter closes, arrows
    /// nudge the font size (a convenience; the panel is click-first).
    fn settings_input(&mut self, event: &KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter) => self.close_settings(),
            Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::ArrowUp) => {
                self.change_font_size(1)
            }
            Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::ArrowDown) => {
                self.change_font_size(-1)
            }
            _ => {}
        }
    }

    /// Cockpit row under the pointer (needs renderer + tab count).
    fn cockpit_row_hit(&self, pos: PhysicalPosition<f64>) -> Option<usize> {
        let r = self.renderer.as_ref()?;
        let w = self.window.as_ref()?;
        let px = w.inner_size();
        r.cockpit_row_at(
            px.width as usize,
            px.height as usize,
            self.tabs.len(),
            pos.x,
            pos.y,
        )
    }

    /// One cockpit row per tab: focused pane's command or idle directory.
    fn cockpit_entries(&self) -> Vec<render::CockpitEntry> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let status = t
                    .panes
                    .get(t.focused)
                    .map(|p| {
                        let m = p.session.meta.lock_or_recover();
                        // BAYAN-012: running_cmd is screen text and the cwd
                        // a program report; both go through chrome_text
                        if !m.running_cmd.is_empty() {
                            format!("▶ {}", chrome_text(&m.running_cmd))
                        } else if let Some(cwd) = &m.cwd {
                            format!("{} · {}", tr("خامل", "idle"), chrome_text(cwd))
                        } else {
                            tr("خامل", "idle").to_string()
                        }
                    })
                    .unwrap_or_default();
                let n = t.panes.len();
                let status = if n > 1 {
                    format!("{status}   ({n} {})", tr("لوحات", "panes"))
                } else {
                    status
                };
                render::CockpitEntry {
                    // the cockpit spans every group, so with more than one
                    // it says which group each agent's tab lives in
                    title: if self.groups.len() > 1 {
                        format!("{} · {}", self.groups.label(t.group), t.label())
                    } else {
                        t.label()
                    },
                    status,
                    busy: t.busy(i == self.active),
                    attention: t.attention,
                    active: i == self.active,
                    progress: t.progress(),
                }
            })
            .collect()
    }

    /// Keyboard handling while the cockpit is open.
    fn cockpit_input(&mut self, event: &KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.cockpit = false,
            Key::Named(NamedKey::Enter) => {
                self.cockpit = false;
                let sel = self.cockpit_sel;
                self.switch_tab(sel);
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.cockpit_sel = self.cockpit_sel.saturating_sub(1);
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.cockpit_sel = (self.cockpit_sel + 1).min(self.tabs.len().saturating_sub(1));
            }
            _ => {}
        }
        self.request_redraw();
    }

    /// Scroll to the previous (dir<0) or next command prompt (OSC 133 marks).
    fn jump_command(&mut self, dir: i32) {
        let Some(s) = self.session() else { return };
        {
            let mut t = s.term.lock();
            let hist = t.grid().history_size();
            let off = t.grid().display_offset();
            let marks: Vec<usize> = {
                let m = s.meta.lock_or_recover();
                m.marks
                    .iter()
                    .filter(|c| c.abs >= m.evicted)
                    .map(|c| (c.abs - m.evicted) as usize)
                    .collect()
            };
            if let Some(new_off) = jump_offset(&marks, hist, off, dir) {
                t.scroll_display(Scroll::Delta(new_off as i32 - off as i32));
            }
        }
        if let Some(id) = self.focused_pane_id() {
            self.drop_smooth(id);
        }
        self.invalidate_focused_links();
        self.request_redraw();
    }

    /// The focused pane's session in the active tab.
    fn session(&self) -> Option<&Session> {
        let t = self.tabs.get(self.active)?;
        t.panes.get(t.focused).map(|p| &p.session)
    }

    fn session_mut(&mut self) -> Option<&mut Session> {
        let t = self.tabs.get_mut(self.active)?;
        let f = t.focused;
        t.panes.get_mut(f).map(|p| &mut p.session)
    }

    /// OSC 52 may write only from the focused pane of the active tab.
    fn osc52_pane_is_focused(&self, id: u64) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        let ids: Vec<u64> = tab.panes.iter().map(|p| p.id).collect();
        crate::osc52_pane_is_focused(&[(tab.focused, ids.as_slice())], 0, id)
    }

    /// (tab index, pane index) for a pane id, wherever it lives.
    fn find_pane(&self, id: u64) -> Option<(usize, usize)> {
        for (ti, t) in self.tabs.iter().enumerate() {
            if let Some(pi) = t.panes.iter().position(|p| p.id == id) {
                return Some((ti, pi));
            }
        }
        None
    }

    fn spawn_pane(&mut self, cwd: Option<&str>) -> Option<Pane> {
        let shell = self.spawn_shell();
        self.spawn_pane_command(cwd, &shell, None)
    }

    /// The shell a new tab starts. Elevated windows use the profile this
    /// process already resolved from its own detection, never `config.shell`.
    fn spawn_shell(&self) -> String {
        elevate::spawn_shell_command(
            self.elevated,
            self.start_profile_command.as_deref(),
            &self.shell_profiles,
            &self.config.shell_program(),
        )
    }

    fn spawn_pane_command(
        &mut self,
        cwd: Option<&str>,
        command: &str,
        raw: Option<&[String]>,
    ) -> Option<Pane> {
        let id = self.next_id;
        self.next_id += 1;
        match Session::spawn(
            self.size,
            self.proxy.clone(),
            id,
            cwd,
            self.config.scrollback_lines(),
            command,
            self.config.kitty_protocol_on(),
            raw,
            self.elevated,
            Arc::clone(&self.image_budget),
            self.config.shell_integration_on(),
        ) {
            Ok(session) => Some(Pane {
                id,
                session,
                command: command.to_string(),
                last_output: None,
                progress: None,
                mux: None,
            }),
            Err(e) => {
                eprintln!("bayan: spawn failed: {e}");
                None
            }
        }
    }

    fn spawn_tab(&mut self, cwd: Option<String>) {
        let shell = self.spawn_shell();
        self.spawn_tab_command(cwd, &shell, None);
    }

    fn spawn_tab_command(&mut self, cwd: Option<String>, command: &str, raw: Option<&[String]>) {
        self.spawn_tab_named(cwd, command, raw, None);
    }

    fn spawn_tab_named(
        &mut self,
        cwd: Option<String>,
        command: &str,
        raw: Option<&[String]>,
        name: Option<String>,
    ) {
        let cwd_for_spawn = cwd.as_deref().and_then(term::accept_local_cwd);
        match self.spawn_pane_command(cwd_for_spawn.as_deref(), command, raw) {
            Some(pane) => self.adopt_tab(cwd, pane, name),
            // The group this tab was being made for is not kept for the
            // next one: no tab opened in it.
            None => self.spawn_group = None,
        }
    }

    /// Put `pane` in a new tab in the group on screen. `name` wins over the
    /// folder title (an SSH tab is named for the host).
    fn adopt_tab(&mut self, cwd: Option<String>, pane: Pane, name: Option<String>) {
        // Same gate as spawn: a restored UNC path must not become the tab
        // title either. accept_local_cwd does not touch the filesystem.
        let cwd = cwd.and_then(|c| term::accept_local_cwd(&c));
        let title = cwd
            .as_deref()
            .and_then(|c| c.rsplit(['\\', '/']).next().map(str::to_string))
            .unwrap_or_else(|| tr("بيان", "Bayan").to_string());
        // a new tab joins the group on screen, or the one being made for it
        let group = self
            .spawn_group
            .take()
            .unwrap_or_else(|| self.active_group());
        // this becomes the active tab without switch_tab, so a button
        // held on the previous tab has to go up first
        self.abandon_mouse();
        self.tabs.push(Tab {
            group,
            panes: vec![pane],
            layout: layout::Node::single(),
            focused: 0,
            title,
            name: name.and_then(|n| clean_tab_name(&n)),
            attention: false,
            broadcast: false,
            zoomed: false,
        });
        self.active = self.tabs.len() - 1;
        self.mru.touch(self.active);
        self.enter_tab();
        self.relayout_active();
        self.sync_focus(None);
        let index = self.active.to_string();
        // A tab the user opened, not bytes from the pane.
        self.emit_lua_event("tab-opened", &[("index", &index)]);
    }

    /// `mux.enabled` is the only way here. While it is false this returns
    /// before any pipe probe or `bayan --mux-server` spawn, the same branch
    /// `connect_if_enabled` tests.
    fn ensure_mux(&mut self) -> bool {
        if !mux::mux_may_run(self.config.mux_enabled(), mux::is_elevated()) {
            return false;
        }
        if self.mux_client.as_ref().is_some_and(|c| c.is_alive()) {
            return true;
        }
        self.mux_client = mux::connect_or_spawn(true).ok();
        self.mux_client.as_ref().is_some_and(|c| c.is_alive())
    }

    fn open_ssh_menu(&mut self) {
        if self.elevated {
            return;
        }
        let hosts = ssh::host_list(&self.config.ssh_hosts, &ssh::read_user_config());
        if hosts.is_empty() {
            self.blocked_link = Some(BlockedLink {
                reason: tr("لا مضيفين محفوظين", "No saved hosts").into(),
                shown: tr(
                    "أضف المضيف في الإعداد أو في ملف الإعداد",
                    "Add a host in config.json or in the ssh config file",
                )
                .into(),
                confirm: None,
                is_app_error: false,
            });
            return;
        }
        self.open_palette(String::new());
        if let Some(p) = self.palette.as_mut() {
            p.ssh_menu = true;
            p.extras = hosts
                .into_iter()
                .map(|h| {
                    (
                        PaletteRow::SshHost(h.token),
                        format!("{} — {}", ssh::label_open(), h.title),
                    )
                })
                .collect();
        }
    }

    fn open_ssh_host(&mut self, token: &str) {
        let dir = ssh::system32_dir();
        let launch = match ssh::prepare_launch(dir.as_deref(), |p| p.is_file(), token) {
            Ok(launch) => launch,
            Err(ssh::SshError::NoClient) => {
                self.blocked_link = Some(BlockedLink {
                    reason: tr(
                        "تعذر العثور على عميل الاتصال",
                        "Could not find the OpenSSH client",
                    )
                    .into(),
                    shown: tr(
                        "العميل يأتي من مجلد النظام",
                        "The client comes from the system directory",
                    )
                    .into(),
                    confirm: None,
                    is_app_error: false,
                });
                return;
            }
            Err(ssh::SshError::BadHost) => {
                self.blocked_link = Some(BlockedLink {
                    reason: tr("اسم المضيف مرفوض", "That host name was rejected").into(),
                    shown: String::new(),
                    confirm: None,
                    is_app_error: false,
                });
                return;
            }
        };
        let cwd = self
            .session()
            .and_then(|s| s.meta.lock_or_recover().spawn_cwd());
        if self.config.mux_enabled() && self.spawn_mux_command(cwd.clone(), &launch) {
            return;
        }
        let before = self.tabs.len();
        // The resolved argv is the process: System32's ssh.exe and its
        // arguments, with no shell wrapper and no second split.
        let argv: Vec<String> = std::iter::once(launch.program.to_string_lossy().into_owned())
            .chain(launch.args.iter().cloned())
            .collect();
        self.spawn_tab_named(cwd, &launch.command_line, Some(&argv), Some(launch.title));
        if self.tabs.len() == before && self.blocked_link.is_none() {
            self.blocked_link = Some(BlockedLink {
                reason: tr("تعذر تشغيل الاتصال", "Could not start the connection").into(),
                shown: link::preview(&launch.command_line, 240),
                confirm: None,
                is_app_error: false,
            });
        }
    }

    /// Start `launch` inside the mux server and attach a tab to it.
    /// False means the server was not used; the caller starts a local tab.
    fn spawn_mux_command(&mut self, cwd: Option<String>, launch: &ssh::SshLaunch) -> bool {
        if !self.ensure_mux() {
            return false;
        }
        let Some(client) = self.mux_client.clone() else {
            return false;
        };
        let spec = mux::SpawnSpec {
            title: launch.title.clone(),
            cwd: cwd.clone().unwrap_or_default(),
            program: launch.program.display().to_string(),
            args: launch.args.clone(),
            cols: self.size.cols.max(1) as u16,
            rows: self.size.rows.max(1) as u16,
        };
        let attached = match client.spawn(spec) {
            Ok(attached) => attached,
            Err(error) => {
                eprintln!("bayan: mux spawn failed: {error}");
                return false;
            }
        };
        let title = launch.title.clone();
        let command = launch.command_line.clone();
        let Some(pane) = self.pane_from_attached(attached, &command) else {
            return false;
        };
        self.adopt_tab(cwd, pane, Some(title));
        true
    }

    fn pane_from_attached(&mut self, attached: mux::Attached, command: &str) -> Option<Pane> {
        let id = self.next_id;
        self.next_id += 1;
        let remote = attached.id;
        let armed = std::sync::Arc::clone(&attached.armed);
        let session = term::Session::from_parts(term::SessionArgs {
            size: self.size,
            proxy: self.proxy.clone(),
            id,
            scrollback: self.config.scrollback_lines(),
            shell: command.to_string(),
            kitty_protocol: self.config.kitty_protocol_on(),
            prompt_nonce: term::NONCE_OFF.to_string(),
            images: Arc::clone(&self.image_budget),
            // the scripts, if any, belong to the server's process
            integration: shellint::IntegrationDir(None),
            io: term::SessionIo {
                reader: attached.reader,
                writer: attached.writer,
                master: None,
                child: None,
                on_resize: Some(attached.on_resize),
                on_kill: Some(attached.on_kill),
            },
        })
        .map_err(|error| {
            eprintln!("bayan: mux pane failed: {error}");
            error
        })
        .ok()?;
        Some(Pane {
            id,
            session,
            command: command.to_string(),
            last_output: None,
            progress: None,
            mux: Some(MuxBind { id: remote, armed }),
        })
    }

    fn focused_is_mux(&self) -> bool {
        self.tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.focused))
            .is_some_and(|p| p.mux.is_some())
    }

    fn detach_focused(&mut self, el: &ActiveEventLoop) {
        if !self.ensure_mux() {
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Some(pane) = tab.panes.get_mut(tab.focused) else {
            return;
        };
        let Some(bind) = pane.mux.as_ref() else {
            return;
        };
        bind.armed
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let remote = bind.id;
        let pi = tab.focused;
        if let Some(client) = &self.mux_client {
            let _ = client.detach(remote);
        }
        self.remove_pane(self.active, pi, el);
    }

    fn open_detached_menu(&mut self) {
        if !self.ensure_mux() {
            return;
        }
        let sessions = self
            .mux_client
            .as_ref()
            .and_then(|c| c.list().ok())
            .unwrap_or_default();
        if sessions.is_empty() {
            self.blocked_link = Some(BlockedLink {
                reason: tr("لا جلسات مفصولة", "No detached sessions").into(),
                shown: String::new(),
                confirm: None,
                is_app_error: false,
            });
            return;
        }
        self.open_palette(String::new());
        if let Some(p) = self.palette.as_mut() {
            p.detached_menu = true;
            p.extras = sessions
                .into_iter()
                .flat_map(|s| {
                    let title = if s.title.is_empty() {
                        s.command.clone()
                    } else {
                        s.title.clone()
                    };
                    [
                        (
                            PaletteRow::Reattach(s.id),
                            format!("{} — {title}", mux::label_reattach()),
                        ),
                        (
                            PaletteRow::KillSession(s.id),
                            format!("{} — {title}", mux::label_kill()),
                        ),
                    ]
                })
                .collect();
        }
    }

    fn reattach_session(&mut self, id: u64) {
        if !self.ensure_mux() {
            return;
        }
        let Some(client) = self.mux_client.clone() else {
            return;
        };
        let attached = match client.reattach(id) {
            Ok(attached) => attached,
            Err(error) => {
                eprintln!("bayan: reattach failed: {error}");
                return;
            }
        };
        let title = attached.title.clone();
        let command = attached.command.clone();
        if let Some(pane) = self.pane_from_attached(attached, &command) {
            self.adopt_tab(None, pane, Some(title));
        }
    }

    fn kill_detached(&mut self, id: u64) {
        if let Some(client) = &self.mux_client {
            client.kill_remote(id);
        }
    }

    /// The window's content area: below the tab bar, inside the padding.
    pub(crate) fn content_rect(&self) -> Option<render::Rect> {
        #[cfg(test)]
        if let Some(rect) = self.test_content {
            return Some(rect);
        }
        let (r, w) = (self.renderer.as_ref()?, self.window.as_ref()?);
        let px = w.inner_size();
        let bar = r.tab_bar_h().round() as i32;
        // the padding setting insets the whole content area evenly
        let pad = self.padding_physical();
        Some((
            pad,
            bar + pad,
            (px.width as i32 - pad * 2).max(1),
            (px.height as i32 - bar - pad * 2).max(1),
        ))
    }

    /// Side-by-side seam, wide enough for the scrollbar when one is shown.
    /// A hidden bar, or no padding to sit in, keeps the one-pixel seam so
    /// the grid (and the ConPTY column count) does not move.
    pub(crate) fn split_gaps(&self) -> layout::Gaps {
        let room = self.scrollbar_gutter();
        if room <= 1 {
            layout::Gaps::HAIRLINE
        } else {
            layout::Gaps {
                beside: room,
                stacked: 1,
            }
        }
    }

    /// Configured thickness in physical pixels (`width` × the window scale).
    pub(crate) fn scrollbar_width_px(&self) -> i32 {
        let scale = self
            .window
            .as_ref()
            .map(|w| w.scale_factor() as f32)
            .unwrap_or(1.0);
        self.config.scrollbar.width_px_at(scale)
    }

    /// Bar thickness that fits in the padding. Never wider than the padding,
    /// so the outer bar cannot enter the grid.
    pub(crate) fn scrollbar_gutter(&self) -> i32 {
        if self.config.scrollbar.side() == scrollbar::Side::Hidden {
            return 0;
        }
        let pad = self.padding_physical();
        if pad <= 0 {
            return 0;
        }
        let width = self.scrollbar_width_px();
        if width <= 1 {
            return 0;
        }
        width.min(pad)
    }

    fn padding_physical(&self) -> i32 {
        let scale = self
            .window
            .as_ref()
            .map(|w| w.scale_factor() as f32)
            .unwrap_or(1.0);
        self.config.padding_px_at(scale)
    }

    /// Pane rects for the active tab within the window's content area.
    fn pane_rects(&self) -> Vec<render::Rect> {
        let (Some(content), Some(t)) = (self.content_rect(), self.tabs.get(self.active)) else {
            return Vec::new();
        };
        if t.zoomed && t.panes.len() > 1 {
            zoom_rects(content, t.panes.len(), t.focused)
        } else {
            t.layout
                .rects_spaced(content, t.panes.len(), self.split_gaps())
        }
    }

    /// The active tab's dividers (none while zoomed: one pane shows).
    pub(crate) fn dividers(&self) -> Vec<layout::Divider> {
        match (self.content_rect(), self.tabs.get(self.active)) {
            (Some(content), Some(t)) if t.panes.len() > 1 && !t.zoomed => {
                t.layout.dividers_spaced(content, self.split_gaps())
            }
            _ => Vec::new(),
        }
    }

    /// Drag divider `i` (an index into `dividers()`) to the pointer: the two
    /// sides of that one split trade size.
    fn drag_divider_to(&mut self, i: usize, pos: PhysicalPosition<f64>) {
        let Some(d) = self.dividers().get(i).cloned() else {
            return;
        };
        let Some(content) = self.content_rect() else {
            return;
        };
        let gaps = self.split_gaps();
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        let p = match d.axis {
            Orientation::Row => pos.x,
            Orientation::Column => pos.y,
        };
        t.layout.drag_spaced(&d, content, p, gaps);
        self.relayout_active();
        self.request_redraw();
    }

    /// Resize every pane of the ACTIVE tab to its rect (background tabs
    /// re-layout when they become active — the standard terminal tradeoff).
    fn relayout_active(&mut self) {
        // Pane rows moved. Cached link spans would underline the wrong cells,
        // and a pixel offset belongs to the old cell height.
        self.link_cache.clear();
        scroll::apply_resize(&mut self.smooth);
        // A wrap changes what the reader sees even when the cursor does not.
        self.bump_reader();
        // resolve "hide the bar with one tab" BEFORE measuring the content
        let hide = self.config.hide_single_tab_on() && self.tabs.len() <= 1;
        if let Some(r) = self.renderer.as_mut() {
            r.set_bar_hidden(hide);
        }
        let rects = self.pane_rects();
        let Some(r) = self.renderer.as_ref() else {
            return;
        };
        let (cw, ch) = (r.cell_w, r.cell_h);
        // Same cell the grid was measured with. `CSI 14 t` multiplies it
        // by each pane's columns and rows.
        term::set_text_area_cell(cw, ch);
        if let Some(t) = self.tabs.get_mut(self.active) {
            for (pane, rect) in t.panes.iter_mut().zip(&rects) {
                if !rect_visible(rect) {
                    continue; // zoomed away: keep the size it had
                }
                let g = TermSize {
                    // the grid only: the scrollbar lives in the padding or
                    // the seam, so it is not a column
                    cols: scrollbar::grid_columns(rect.2, cw),
                    rows: ((rect.3 as f32 / ch) as usize).max(3),
                };
                pane.session.resize(g);
            }
        }
    }

    /// Split the active tab (EasyTer: Ctrl+Shift+E side-by-side, O stacked).
    fn split(&mut self, orientation: Orientation) {
        // side-by-side and stacked nest, up to MAX_PANES_PER_TAB
        if self
            .tabs
            .get(self.active)
            .is_none_or(|t| t.panes.len() >= MAX_PANES_PER_TAB)
        {
            return;
        }
        let cwd = self
            .session()
            .and_then(|s| s.meta.lock_or_recover().spawn_cwd());
        let Some(pane) = self.spawn_pane(cwd.as_deref()) else {
            return;
        };
        if let Some(t) = self.tabs.get_mut(self.active) {
            // the FOCUSED pane splits the way that was asked, and only it:
            // side by side and stacked mix freely in one tab
            let new = t.panes.len();
            t.layout.split(t.focused, orientation, new);
            t.panes.push(pane);
            t.focused = t.panes.len() - 1;
            // the new pane has to be seen: a split ends the zoom
            t.zoomed = false;
        }
        self.relayout_active();
        self.sync_focus(None);
        self.request_redraw();
    }

    /// Close a whole tab (the tab-bar ×). Confirms when any pane still runs.
    fn request_close_tab(&mut self, ti: usize, el: &ActiveEventLoop) {
        let Some(t) = self.tabs.get(ti) else {
            return;
        };
        let running = t
            .panes
            .iter()
            .any(|p| !p.session.meta.lock_or_recover().running_cmd.is_empty());
        if self.config.confirm_close_on() && running {
            self.pending_close = Some(CloseTarget::Tab(ti));
            self.request_redraw();
            return;
        }
        self.remove_tab(ti, el);
    }

    /// Kill every pane in a tab and drop the tab; the last tab closes Bayan.
    fn remove_tab(&mut self, ti: usize, el: &ActiveEventLoop) {
        self.abandon_mouse();
        let Some(mut t) = (ti < self.tabs.len()).then(|| self.tabs.remove(ti)) else {
            return;
        };
        if matches!(self.pending_close, Some(CloseTarget::Tab(i)) if i == ti)
            || matches!(self.pending_close, Some(CloseTarget::Pane(i, _)) if i == ti)
        {
            self.pending_close = None;
        }
        let removed: Vec<u64> = t.panes.iter().map(|p| p.id).collect();
        dismiss_paste_for(&mut self.pending_paste, &removed);
        for mut pane in t.panes.drain(..) {
            pane.session.kill();
        }
        if self.tabs.is_empty() {
            self.quit(el);
            return;
        }
        self.after_tab_removed(ti, t.group);
        self.relayout_active();
        self.sync_focus(None);
        self.request_redraw();
    }

    /// The per-tab resets for whichever tab just came on screen.
    fn enter_tab(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.attention = false; // you're looking at it now
        }
        self.search = None; // the bar belongs to the tab that opened it
    }

    /// Remove one pane; a tab with no panes closes, the last tab closes Bayan.
    fn remove_pane(&mut self, ti: usize, pi: usize, el: &ActiveEventLoop) {
        // before the pane is gone, so a tracked button can still be released
        self.abandon_mouse();
        let Some(t) = self.tabs.get_mut(ti) else {
            return;
        };
        if pi >= t.panes.len() {
            return;
        }
        // Pane indices can shift when a background shell exits. Dismiss an
        // index-based confirmation rather than letting it target a neighbour.
        if matches!(
            self.pending_close,
            Some(CloseTarget::Pane(..)) | Some(CloseTarget::Tab(_))
        ) {
            self.pending_close = None;
        }
        let removed = t.panes[pi].id;
        dismiss_paste_for(&mut self.pending_paste, &[removed]);
        let mut pane = t.panes.remove(pi);
        t.layout.remove(pi);
        pane.session.kill();
        if t.panes.is_empty() {
            let gone = self.tabs.remove(ti);
            if self.tabs.is_empty() {
                self.quit(el);
                return;
            }
            self.after_tab_removed(ti, gone.group);
        } else {
            if t.focused >= t.panes.len() {
                t.focused = t.panes.len() - 1;
            }
            // the layout changed under the zoom: show the panes that are left
            t.zoomed = false;
            if t.panes.len() < 2 {
                t.broadcast = false; // nothing left to broadcast to
            }
        }
        self.relayout_active();
        self.sync_focus(None);
        self.request_redraw();
    }

    /// Tab `ti` (of group `gid`) has just left `self.tabs`, which is not
    /// empty: pick what is shown next, staying in the group while it has
    /// tabs (`groups::after_close`), and drop the group once it has none.
    fn after_tab_removed(&mut self, ti: usize, gid: u32) {
        self.mru.removed(ti);
        let (shifted, was_active) = active_after_remove(self.active, ti, self.tabs.len());
        self.active = groups::after_close(was_active, shifted, gid, &self.tab_groups(), &self.mru);
        self.mru.touch(self.active);
        self.groups.prune(&self.tab_groups());
        if was_active {
            self.enter_tab(); // on screen now, perhaps from another group
        }
    }

    /// Each tab's group id, in tab order.
    fn tab_groups(&self) -> Vec<u32> {
        self.tabs.iter().map(|t| t.group).collect()
    }

    /// The group on screen: the active tab's.
    fn active_group(&self) -> u32 {
        self.tabs
            .get(self.active)
            .map_or_else(|| self.groups.first(), |t| t.group)
    }

    /// The tabs the bar shows (the active group's), as indices into
    /// `self.tabs`, and where the active tab sits among them.
    fn visible_tabs(&self) -> (Vec<usize>, usize) {
        let vis = groups::members(&self.tab_groups(), self.active_group());
        let pos = vis.iter().position(|&i| i == self.active).unwrap_or(0);
        (vis, pos)
    }

    /// The bar's labels, the active group's tabs only — the same list the
    /// renderer measured, so hit tests land where the tabs were drawn.
    pub(crate) fn visible_labels(&self) -> (Vec<String>, usize) {
        let (vis, pos) = self.visible_tabs();
        (vis.iter().map(|&i| self.tabs[i].label()).collect(), pos)
    }

    /// The group chip's text and its amber dot, while there is more than
    /// one group: one group is the plain tab bar, unchanged.
    fn group_chip(&self) -> Option<(String, bool)> {
        if self.groups.len() < 2 {
            return None;
        }
        let g = self.active_group();
        let elsewhere = self.tabs.iter().any(|t| t.group != g && t.attention);
        Some((self.groups.label(g), elsewhere))
    }

    /// Show group `gid` at the tab it was last on (else its first tab).
    fn show_group(&mut self, gid: u32) {
        let members = groups::members(&self.tab_groups(), gid);
        let Some(&first) = members.first() else {
            return;
        };
        let target = self.mru.most_recent(&members).unwrap_or(first);
        self.switch_tab(target);
    }

    /// Ctrl+Shift+PageDown / PageUp and the chip: the next or previous group.
    fn step_group(&mut self, back: bool) {
        let target = self.groups.neighbor(self.active_group(), back);
        if target != self.active_group() {
            self.show_group(target);
        }
    }

    /// Ctrl+Shift+G: a new group, opened on one fresh tab in the active
    /// pane's directory (the same start NewTab gives).
    fn new_group(&mut self) {
        let cwd = self
            .session()
            .and_then(|s| s.meta.lock_or_recover().spawn_cwd());
        self.spawn_group = Some(self.groups.add());
        self.spawn_tab(cwd);
        self.spawn_group = None;
        // a failed spawn leaves the new group empty: drop it again
        self.groups.prune(&self.tab_groups());
        self.request_redraw();
    }

    /// Move the active tab into group `gid` (`None`: a new group). The
    /// view follows the tab; a group it leaves empty is dropped.
    fn move_tab_to_group(&mut self, gid: Option<u32>) {
        let gid = match gid {
            Some(g) if self.groups.contains(g) => g,
            Some(_) => return,
            None => self.groups.add(),
        };
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        t.group = gid;
        self.groups.prune(&self.tab_groups());
        self.mru.touch(self.active);
        self.save_state();
        self.request_redraw();
    }

    fn open_rename_group(&mut self) {
        let current = self
            .groups
            .name(self.active_group())
            .unwrap_or_default()
            .to_string();
        self.open_palette(current);
        if let Some(p) = self.palette.as_mut() {
            p.rename = Some(RenameTarget::Group);
        }
    }

    fn rename_group(&mut self, name: Option<String>) {
        let g = self.active_group();
        self.groups
            .set_name(g, name.as_deref().and_then(clean_tab_name));
        self.save_state();
        self.request_redraw();
    }

    fn switch_tab(&mut self, idx: usize) {
        self.show_tab(idx);
        self.mru.touch(self.active);
    }

    /// Show tab `idx` without touching the MRU history — the Ctrl+Tab walk
    /// passes through tabs; only the one it ends on (Ctrl released) counts.
    fn show_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() || idx == self.active {
            return;
        }
        self.abandon_mouse();
        self.active = idx;
        self.enter_tab();
        self.relayout_active(); // this tab's layout may differ
        self.sync_focus(None);
        self.request_redraw();
    }

    fn save_state(&mut self) -> bool {
        // Every caller (close, rename, group assign) comes through here.
        // An elevated process must not create or replace session.json.
        if !elevate::may_write_bayan(self.elevated) {
            return true;
        }
        // Closing while discovery is pending must not replace the saved
        // session with an empty window that has not restored it yet.
        if self.startup_pending {
            return true;
        }
        let Some(path) = state_path() else {
            return true;
        };
        let state = SavedState {
            tabs: self
                .tabs
                .iter()
                .map(|t| {
                    let (orientation, weights) = saved_split(&t.layout);
                    SavedTab {
                        cwds: t
                            .panes
                            .iter()
                            .map(|p| {
                                // don't persist a path we would refuse to start in.
                                // BAYAN-015: a poisoned meta lock is this pane's
                                // state, not a reason to abort the save.
                                p.session
                                    .meta
                                    .lock_or_recover()
                                    .cwd
                                    .as_deref()
                                    .and_then(term::accept_local_cwd)
                            })
                            .collect(),
                        weights,
                        orientation,
                        focused: t.focused,
                        layout: Some(t.layout.clone()),
                        name: t.name.clone(),
                        group: self.groups.position(t.group).unwrap_or(0),
                        shells: saved_shells(
                            t.panes.iter().map(|p| p.command.as_str()),
                            &self.config.shell_program(),
                        ),
                    }
                })
                .collect(),
            active: self.active,
            groups: if self.groups.len() > 1 {
                self.groups.iter().map(|g| g.name.clone()).collect()
            } else {
                // one group saves as before: the file stays readable by
                // a build without groups, and its name (if any) is kept
                self.groups
                    .iter()
                    .filter_map(|g| g.name.clone().map(Some))
                    .collect()
            },
            window: self.normal_geom.map(|g| WindowGeom {
                maximized: self.window.as_ref().is_some_and(|w| w.is_maximized()),
                ..g
            }),
        };
        let result = serde_json::to_vec_pretty(&state)
            .map_err(std::io::Error::other)
            .and_then(|json| persist::write(&path, &json));
        if let Err(error) = result {
            self.report_error(
                tr("تعذر حفظ الجلسة", "Could not save the session"),
                &error.to_string(),
            );
            return false;
        }
        true
    }

    /// Note the window's current rectangle if it is in its normal state
    /// (moved or resized by the user, not maximized).
    fn track_geometry(&mut self) {
        let Some(w) = &self.window else { return };
        if w.is_maximized() || w.is_minimized().unwrap_or(false) {
            return;
        }
        let (Ok(pos), size) = (w.outer_position(), w.inner_size()) else {
            return;
        };
        if size.width > 0 && size.height > 0 {
            self.normal_geom = Some(WindowGeom {
                x: pos.x,
                y: pos.y,
                w: size.width,
                h: size.height,
                maximized: false,
            });
        }
    }

    /// Restore only after the background shell inventory is available.
    fn start_sessions(&mut self) {
        if !self.startup_pending
            || self.window.is_none()
            || (self.config.restore_tabs.unwrap_or(false) && !self.shell_profiles_ready)
        {
            return;
        }
        self.startup_pending = false;
        // the PTY + conhost + PowerShell chain is the slowest dependency:
        // start it immediately with a default grid; the exact grid follows
        // once cell metrics exist (EasyTer starts 110x32 the same way).
        // Start one fresh tab unless saved-tab restoration is explicitly enabled.
        // `#45` opens `--cwd` as the active tab after any restored tabs. A
        // profile or a `--` command rides on that same tab; with neither, and
        // no folder, startup is unchanged.
        let explicit = self.start_dir.is_some()
            || self.start_profile_command.is_some()
            || self.start_command.is_some();
        if explicit {
            // Elevated: do not replay session.json. Its commands and folders
            // were written by a process that was not elevated.
            if elevate::may_restore_saved_tabs(
                self.elevated,
                self.config.restore_tabs.unwrap_or(false),
                load_saved_state().is_some_and(|s| !s.tabs.is_empty()),
            ) {
                self.restore_state();
            }
            let default = self.spawn_shell();
            let process = cli::launch_process(
                self.start_profile_command.as_deref(),
                self.start_command.as_deref(),
                &default,
            );
            self.spawn_tab_command(
                self.start_dir.clone(),
                &process.command_line,
                process.raw_argv.as_deref(),
            );
        } else if self.elevated {
            let shell = self.spawn_shell();
            self.spawn_tab_command(None, &shell, None);
        } else {
            self.restore_state();
        }
        // debug hook: BAYAN_SPLIT=1 opens the first tab pre-split, so the
        // pane machinery can be verified without injecting any input
        if dev_hook("BAYAN_SPLIT").is_some() {
            self.split(Orientation::Row);
        }
        // debug hook: BAYAN_GROUPS=1 adds a second tab to the first group,
        // then a second group of one tab, which stays on screen — the chip,
        // the per-group bar and the group rows, reachable without input
        if dev_hook("BAYAN_GROUPS").is_some() {
            self.spawn_tab(None);
            self.new_group();
        }
        // debug hook: BAYAN_SHELLS=1 opens the tab bar's shell menu as a
        // right-click on the bar would, anchored inside the bar; it fills
        // in when shell detection reports
        if dev_hook("BAYAN_SHELLS").is_some() {
            self.cursor_pos = PhysicalPosition::new(240.0, 20.0);
            self.open_shell_menu();
        }
        if dev_hook("BAYAN_COCKPIT").is_some() {
            self.cockpit = true;
            self.cockpit_sel = self.active;
        }
        if dev_hook("BAYAN_GUARD").is_some() {
            self.arm_pending_paste("echo one\necho two\necho three\n".into(), None);
        }
        if dev_hook("BAYAN_SETTINGS").is_some() {
            self.settings = Some(0);
        }
        if dev_hook("BAYAN_SHORTCUTS").is_some() {
            self.shortcuts = Some(ShortcutsState::default());
        }
        // BAYAN_TYPE=<text>: feed text into the first pane's PTY — the
        // visual-verification hook (ligatures, shaping) with no keyboard
        if let Some(v) = dev_hook("BAYAN_TYPE") {
            if let Some(t) = v.to_str() {
                let t = t.to_string();
                if let Some(s) = self.session_mut() {
                    s.write(t.as_bytes());
                }
            }
        }
        // BAYAN_PASTE=<text>: put text on the clipboard and right-click the
        // focused pane once the renderer exists, so right-click paste can
        // be verified without injecting mouse input. It replaces whatever
        // was on the clipboard.
        if let Some(v) = dev_hook("BAYAN_PASTE") {
            self.debug_paste = v.to_str().map(str::to_string);
        }
        // BAYAN_POSH=1: open the prompt picker at startup (capture without keys)
        if dev_hook("BAYAN_POSH").is_some() {
            self.open_posh();
        }
        // BAYAN_PALETTE=<query|1>: open the palette at startup, so it can be
        // captured without injecting keystrokes (the debug-hook rule)
        if let Some(v) = dev_hook("BAYAN_PALETTE") {
            let q = v.to_str().unwrap_or("").to_string();
            self.open_palette(if q == "1" { String::new() } else { q });
        }
        // BAYAN_TOAST=<title|1>: fire a sample toast at startup (M19
        // verification; a custom value becomes the title, so direction
        // experiments run without rebuilding)
        if let Some(v) = dev_hook("BAYAN_TOAST") {
            if let Some(h) = self.hwnd() {
                let t = v.to_str().unwrap_or("1");
                let title = if t == "1" {
                    tr("اكتمل الأمر", "Command finished")
                } else {
                    t
                };
                let ok = toast::show(
                    h,
                    title,
                    tr(
                        "بيان — إشعار تجريبي (M19)",
                        "Bayan — test notification (M19)",
                    ),
                );
                eprintln!("bayan: BAYAN_TOAST show accepted={ok}");
            }
        }
        // BAYAN_PICK_THEME=<n>: open settings and apply theme n, so a click's
        // live effect can be verified in a screenshot without input injection
        if let Some(v) = dev_hook("BAYAN_PICK_THEME") {
            self.settings = Some(0);
            if let Some(n) = v.to_str().and_then(|s| s.parse::<usize>().ok()) {
                self.apply_theme(n);
            }
        }
        crate::prof::mark("session spawned");
        // the window exists now; if it is already focused and a pane has
        // 1004, say so. has_focus is the same check a later rebase uses.
        self.sync_focus(None);
        if self.renderer.is_some() {
            if let Some(text) = self.debug_paste.take() {
                self.debug_right_click_paste(text);
            }
        }
        self.request_redraw();
    }

    fn restore_state(&mut self) {
        if !self.config.restore_tabs.unwrap_or(false) {
            self.spawn_tab(None);
            return;
        }
        let saved = load_saved_state();
        match saved {
            Some(state) if !state.tabs.is_empty() => {
                let group_count = state.groups.len().max(1);
                self.groups = groups::Groups::restored(
                    state
                        .groups
                        .iter()
                        .map(|n| n.as_deref().and_then(clean_tab_name))
                        .collect(),
                );
                // BAYAN-014: a session.json with hundreds of tabs (a bug, a
                // hand edit, a planted file) must not spawn hundreds of shells
                let default = self.config.shell_program();
                let installed = self.shell_profiles.clone();
                for mut st in state.tabs.into_iter().take(MAX_RESTORED_TABS) {
                    let before = self.tabs.len();
                    let mut cwds = std::mem::take(&mut st.cwds).into_iter();
                    let mut shells = std::mem::take(&mut st.shells).into_iter();
                    let mut shell =
                        || restored_shell(shells.next().flatten(), &default, &installed);
                    // an index past the saved groups (a hand edit) joins
                    // the first group rather than inventing one
                    let group = if st.group < group_count { st.group } else { 0 };
                    self.spawn_group = Some(group as u32);
                    let cwd = cwds.next().flatten();
                    let first = shell();
                    self.spawn_tab_command(cwd.clone(), &first, None);
                    if self.tabs.len() == before && first != default {
                        // the saved shell is gone (PowerShell 7 uninstalled,
                        // a WSL distro removed): the tab still comes back
                        self.spawn_group = Some(group as u32);
                        self.spawn_tab(cwd);
                    }
                    self.spawn_group = None;
                    // N-05: the first shell of this saved tab is what creates
                    // it. If that spawn fails, its extra panes, layout, focus
                    // and name must not land on the previous tab.
                    let started = self.tabs.len() == before + 1;
                    let panes_now = self.tabs.last().map_or(0, |t| t.panes.len());
                    let plan = plan_restored_tab(started, panes_now, cwds.len());
                    if !plan.apply {
                        continue; // its shell failed: no tab to fill in
                    }
                    for cwd in cwds.take(plan.extra) {
                        // re-split with the saved axis; weights come after
                        let cmd = shell();
                        let pane = self
                            .spawn_pane_command(cwd.as_deref(), &cmd, None)
                            .or_else(|| self.spawn_pane(cwd.as_deref()));
                        if let Some(pane) = pane {
                            if let Some(t) = self.tabs.last_mut() {
                                t.panes.push(pane);
                            }
                        }
                    }
                    if let Some(t) = self.tabs.last_mut() {
                        t.layout = restored_layout(&st, t.panes.len());
                        t.focused = st.focused.min(t.panes.len().saturating_sub(1));
                        t.name = st.name.as_deref().and_then(clean_tab_name);
                    }
                }
                if self.tabs.is_empty() {
                    self.groups = groups::Groups::new();
                    self.spawn_tab(None);
                    return;
                }
                // groups whose every tab failed to come back are dropped
                self.groups.prune(&self.tab_groups());
                self.active = state.active.min(self.tabs.len().saturating_sub(1));
                self.mru.touch(self.active);
                self.relayout_active();
            }
            _ => self.spawn_tab(None),
        }
    }

    fn term_mode(&self) -> TermMode {
        self.session()
            .map_or(TermMode::empty(), |s| *s.term.lock().mode())
    }

    fn note_held_away(&mut self, event: &KeyEvent) {
        let key = event.physical_key;
        if !self.keys_held_away.contains(&key) {
            self.keys_held_away.push(key);
        }
    }

    /// `true` when this key's press never reached the PTY.
    fn take_held_away(&mut self, event: &KeyEvent) -> bool {
        let key = event.physical_key;
        if let Some(i) = self.keys_held_away.iter().position(|k| *k == key) {
            self.keys_held_away.swap_remove(i);
            true
        } else {
            false
        }
    }

    fn encode_key_event(&self, event: &KeyEvent, released: bool) -> Option<Vec<u8>> {
        self.encode_key_event_for(event, released, self.term_mode())
    }

    /// The same encoding against another pane's modes: a broadcast key
    /// reaches vim in application-cursor mode as vim expects it, and a
    /// pane that pushed kitty flags gets CSI u while its neighbour does not.
    fn encode_key_event_for(
        &self,
        event: &KeyEvent,
        released: bool,
        mode: TermMode,
    ) -> Option<Vec<u8>> {
        use winit::keyboard::PhysicalKey;
        use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
        let m = self.modifiers;
        let unmod = event.key_without_modifiers();
        let physical = match event.physical_key {
            PhysicalKey::Code(code) => Some(code),
            _ => None,
        };
        keys::encode_input(&keys::EncodeInput {
            key: &event.logical_key,
            unmod: &unmod,
            physical,
            location: event.location,
            text: event.text.as_deref(),
            shift: m.shift_key(),
            alt: m.alt_key(),
            ctrl: m.control_key(),
            super_key: m.super_key(),
            repeat: event.repeat,
            released,
            mode: keys::mode_for_keys(mode, self.config.kitty_protocol_on()),
            physical_letter: key_letter(event),
            base_layout_fallback: self.config.kitty_base_layout_fallback_on(),
        })
    }

    /// Claude mode: auto = a full-screen TUI is active AND the command that
    /// started it is Claude itself (vim/less emit logical Arabic — never
    /// reverse those). F2 overrides manually.
    ///
    /// Explicit BDSM (`CSI 8 l`), when `bidi.honor_bdsm` is set, wins over
    /// both: the application already ordered the text, so this pane does
    /// not reorder.
    fn claude_active(&self) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        let Some(pane) = tab.panes.get(tab.focused) else {
            return false;
        };
        self.pane_claude(pane)
    }

    /// Claude restoration for one pane. Term lock, then meta, never both:
    /// the same order as every other pane read.
    fn pane_claude(&self, pane: &Pane) -> bool {
        let term_alt = pane
            .session
            .term
            .lock()
            .mode()
            .contains(TermMode::ALT_SCREEN);
        let (bidi_state, running, pane_mode, tracked_alt) = {
            let m = pane.session.meta.lock_or_recover();
            (
                m.bidi.clone(),
                m.running_cmd.clone(),
                m.bidi_reorder,
                m.alt_track.on(),
            )
        };
        // Idle explicit mode (a command that forgot to reset) must not
        // suppress restoration at the prompt. A live explicit mode still does.
        // The pane's BiDi mode (off / alt screen) is the same kind of gate.
        // `alt_off` also counts ?47 and ?1047, which do not set the term bit.
        let mode = pane_mode.unwrap_or_else(|| self.config.bidi_mode());
        let alt = mode.effective_alt(term_alt, tracked_alt);
        let (reorder, _) =
            bidi_state.presentation(mode, self.config.honor_bdsm(), !running.is_empty(), alt);
        if !reorder {
            return false;
        }
        if !self.auto_follow {
            return self.claude_manual;
        }
        bidi::cmd_is_claude(&running)
    }

    /// Pixel offset of one pane. Zero on the alt screen and when smooth
    /// scroll is off, so those paths stay on `cell_at`'s rows.
    fn pane_offset_px(&self, pane_id: u64, cell_h: f32) -> i32 {
        if !self.config.smooth_scroll_on() || self.mode_of(pane_id).contains(TermMode::ALT_SCREEN) {
            return 0;
        }
        let sub = self.smooth.get(&pane_id).map(|s| s.subline).unwrap_or(0.0);
        scroll::offset_px(sub, cell_h)
    }

    /// Viewport row under a pane-local y, including `-1` for the history
    /// line a smooth scroll has pulled into the pane.
    fn signed_row(&self, pane_id: u64, local_y: f64, cell_h: f64, rows: usize) -> i32 {
        scroll::locate_row(
            local_y,
            cell_h,
            self.pane_offset_px(pane_id, cell_h as f32) as f64,
            rows,
        )
    }

    fn close_message(&self, target: CloseTarget) -> String {
        match target {
            CloseTarget::Pane(ti, pi) => {
                let cmd = chrome_text(&self.running_command(ti, pi).unwrap_or_default());
                screen_reader::close_pane_message(&cmd)
            }
            CloseTarget::Tab(ti) => {
                let n = self
                    .tabs
                    .get(ti)
                    .map(|t| {
                        t.panes
                            .iter()
                            .filter(|p| !p.session.meta.lock_or_recover().running_cmd.is_empty())
                            .count()
                    })
                    .unwrap_or(0);
                screen_reader::close_count_message(n, true)
            }
            CloseTarget::Window => screen_reader::close_count_message(self.running_count(), false),
        }
    }

    /// Push a full tree when a client is connected and the stamp changed.
    /// Before the first client this returns without taking the terminal
    /// lock. Afterwards an unchanged frame does not build, and a burst is
    /// at most one build per [`screen_reader::TREE_MIN_MS`], with the last stamp
    /// delivered on [`Self::reader_tree_at`].
    fn publish_reader(&mut self) {
        if !self.reader_seen_client || self.reader.is_none() {
            return;
        }
        let now_ms = self.reader_origin.elapsed().as_millis() as u64;
        let announce_due = self.reader_deadline.is_some_and(|t| Instant::now() >= t);
        let fp = self.reader_fp();
        let pace = self.reader_pace.consider(fp, now_ms);
        let build = pace.publish || announce_due;
        if build && !pace.publish {
            self.reader_pace.note_published(fp, now_ms);
        }
        if build {
            let Some(mut adapter) = self.reader.take() else {
                return;
            };
            adapter.update_if_active(|| self.reader_update(now_ms));
            self.reader = Some(adapter);
            self.reader_tree_at = None;
        } else {
            self.reader_tree_at = pace
                .wake_ms
                .map(|ms| self.reader_origin + std::time::Duration::from_millis(ms));
        }
    }

    /// Cursor, selection, focus, tabs, palette, and card. Not the cells.
    fn reader_fp(&self) -> u64 {
        let mut h = screen_reader::stamp_mix(self.reader_gen, self.active as u64);
        h = screen_reader::stamp_mix(h, u64::from(lang::rtl()));
        let (vis, _) = self.visible_tabs();
        for i in vis {
            h = screen_reader::stamp_str(h, &self.tabs[i].label());
            h = screen_reader::stamp_mix(h, u64::from(i == self.active));
        }
        if let Some(tab) = self.tabs.get(self.active) {
            h = screen_reader::stamp_mix(h, tab.focused as u64);
            for pane in &tab.panes {
                let term = pane.session.term.lock();
                h = screen_reader::stamp_mix(h, screen_reader::pane_stamp(&term));
            }
        }
        h = match &self.palette {
            None => screen_reader::stamp_mix(h, 0),
            Some(p) => {
                let mut h = screen_reader::stamp_mix(h, 1);
                h = screen_reader::stamp_mix(h, p.sel as u64);
                h = screen_reader::stamp_mix(h, u64::from(p.menu.is_some()));
                h = screen_reader::stamp_mix(h, u64::from(p.shells_menu));
                h = screen_reader::stamp_str(h, &p.query);
                screen_reader::stamp_mix(h, p.extras.len() as u64)
            }
        };
        h = match self.pending_close {
            None => screen_reader::stamp_mix(h, 0),
            Some(CloseTarget::Pane(ti, pi)) => {
                let h = screen_reader::stamp_mix(h, 1);
                let h = screen_reader::stamp_mix(h, ti as u64);
                screen_reader::stamp_mix(h, pi as u64)
            }
            Some(CloseTarget::Tab(ti)) => {
                let h = screen_reader::stamp_mix(h, 2);
                screen_reader::stamp_mix(h, ti as u64)
            }
            Some(CloseTarget::Window) => screen_reader::stamp_mix(h, 3),
        };
        if let Some(block) = &self.blocked_link {
            h = screen_reader::stamp_str(h, &block.reason);
            h = screen_reader::stamp_str(h, &block.shown);
            h = screen_reader::stamp_mix(h, u64::from(block.confirm.is_some()));
        } else {
            h = screen_reader::stamp_mix(h, 0);
        }
        if let Some(paste) = &self.pending_paste {
            h = screen_reader::stamp_mix(h, paste.lines as u64);
            h = screen_reader::stamp_mix(h, paste.chars as u64);
            h = screen_reader::stamp_str(h, &paste.reason);
        }
        h
    }

    fn bump_reader(&mut self) {
        self.reader_gen = self.reader_gen.wrapping_add(1);
    }

    fn reader_update(&mut self, now_ms: u64) -> accesskit::TreeUpdate {
        let view = self.reader_view(now_ms);
        self.reader_deadline = self
            .reader_gate
            .flush_at_ms()
            .map(|ms| self.reader_origin + std::time::Duration::from_millis(ms));
        screen_reader::build_tree(&view)
    }

    fn reader_view(&mut self, now_ms: u64) -> screen_reader::View {
        let (vis, _) = self.visible_tabs();
        let tabs: Vec<screen_reader::TabItem> = vis
            .iter()
            .map(|&i| screen_reader::TabItem {
                index: i,
                name: self.tabs[i].label(),
                selected: i == self.active,
            })
            .collect();
        let active = self.active;
        let focused = self.tabs.get(active).map(|t| t.focused).unwrap_or(0);
        let mut bodies = Vec::new();
        let mut focused_lines = Vec::new();
        let mut focused_id = None;
        let mut finished = None;
        let mut running = String::new();
        if let Some(tab) = self.tabs.get(active) {
            let count = tab.panes.len();
            for (pi, pane) in tab.panes.iter().enumerate() {
                let claude = self.pane_claude(pane);
                // Copy under the lock, then restore BiDi and build nodes
                // after it is released. A reader must not stall the PTY.
                let raw = {
                    let term = pane.session.term.lock();
                    screen_reader::copy_pane(&term)
                };
                let text = screen_reader::pane_text(raw, claude);
                if pi == focused {
                    focused_id = Some(pane.id);
                    focused_lines = text.lines.iter().map(|line| line.text.clone()).collect();
                    let meta = pane.session.meta.lock_or_recover();
                    running = meta.running_cmd.clone();
                    finished = meta
                        .marks
                        .iter()
                        .rev()
                        .find(|mark| mark.exit.is_some())
                        .map(|mark| (mark.abs, mark.exit));
                }
                bodies.push(screen_reader::PaneItem {
                    index: pi,
                    name: screen_reader::pane_name(pi, count),
                    text,
                });
            }
        }
        if focused_id != self.reader_pane {
            self.reader_gate.reset();
            self.reader_pane = focused_id;
            self.reader_mark = None;
            self.reader_mark_ready = false;
            self.reader_cmd.clear();
            self.reader_bodies.clear();
        }
        if !running.is_empty() {
            self.reader_cmd = running;
        }
        if let Some((abs, exit)) = finished {
            if self.reader_mark != Some(abs) {
                let announce = self.reader_mark_ready;
                self.reader_mark = Some(abs);
                if announce {
                    self.reader_gate
                        .command_finished(exit, &self.reader_cmd, now_ms);
                }
            }
        }
        self.reader_mark_ready = true;
        let update = self.reader_gate.push(&focused_lines, now_ms);
        // A selection is the user's, not the flood's: show it immediately.
        let selecting = bodies.iter().any(|pane| pane.text.selection.is_some());
        let panes =
            if selecting || update.refresh_document || self.reader_bodies.len() != bodies.len() {
                self.reader_bodies = bodies;
                self.reader_bodies.clone()
            } else {
                self.reader_bodies.clone()
            };
        if let Some(live) = update.live.clone() {
            let on_screen = self
                .reader_live
                .as_deref()
                .unwrap_or(screen_reader::live_name());
            self.reader_live = Some(screen_reader::live_label(on_screen, live));
        }
        let palette_snap = self.palette.as_ref().map(|p| {
            (
                p.menu.is_some(),
                p.shells_menu,
                p.shell_arrow,
                p.query.clone(),
                p.sel,
            )
        });
        let palette = palette_snap.map(|(menu, shells_menu, shell_arrow, query, selected)| {
            let (rows, row_names) = if shells_menu {
                let built = self.shell_menu(shell_arrow).rows;
                let q = query.trim().to_lowercase();
                let built: Vec<_> = built
                    .into_iter()
                    .filter(|row| q.is_empty() || row.label.to_lowercase().contains(&q))
                    .collect();
                let names = built.iter().map(|row| row.announce.clone()).collect();
                let labels = built.into_iter().map(|row| row.label).collect();
                (labels, names)
            } else {
                (
                    self.palette_rows(&query)
                        .into_iter()
                        .map(|(_, label)| label)
                        .collect(),
                    Vec::new(),
                )
            };
            screen_reader::PaletteItem {
                name: if shells_menu {
                    shellmenu::menu_label()
                } else if menu {
                    screen_reader::menu_name()
                } else {
                    screen_reader::palette_name()
                }
                .to_string(),
                query,
                rows,
                row_names,
                selected,
                menu,
                shell_menu: shell_arrow,
            }
        });
        let shell_button = Some(screen_reader::ShellButton {
            label: shellmenu::button_label().to_string(),
            focused: self.shell_arrow_focused && self.palette.is_none(),
        });
        let dialog = self.reader_dialog();
        screen_reader::View {
            tabs,
            active_tab: active,
            panes,
            focused_pane: focused,
            live: self
                .reader_live
                .clone()
                .unwrap_or_else(|| screen_reader::live_name().to_string()),
            palette,
            dialog,
            shell_button,
        }
    }

    fn reader_dialog(&self) -> Option<screen_reader::DialogItem> {
        if let Some(target) = self.pending_close {
            let name = self.close_message(target);
            return Some(screen_reader::DialogItem {
                body: name.clone(),
                name,
            });
        }
        if let Some(block) = &self.blocked_link {
            if block.is_app_error {
                return Some(screen_reader::DialogItem {
                    name: block.reason.clone(),
                    body: block.shown.clone(),
                });
            }
            let name = format!(
                "{} — {}",
                link::card_title_prefix(block.confirm.is_some()),
                block.reason
            );
            let (ar, en) = link::card_key_lines(block.confirm.is_some());
            return Some(screen_reader::DialogItem {
                name,
                body: format!("{}\n{ar}\n{en}", block.shown),
            });
        }
        let paste = self.pending_paste.as_ref()?;
        let preview = card_preview(paste.kind, &paste.text);
        let shown = preview.first().map(String::as_str).unwrap_or("");
        Some(screen_reader::DialogItem {
            name: screen_reader::paste_dialog_name().to_string(),
            body: format!("{}\n{shown}", paste.reason),
        })
    }

    /// Pixel position -> (pane index, viewport row, col, cell side).
    /// None inside the tab bar, the padding, or a divider gap.
    fn pane_cell_at(&self, pos: PhysicalPosition<f64>) -> Option<(usize, i32, usize, Side)> {
        let r = self.renderer.as_ref()?;
        let rects = self.pane_rects();
        let hit = mouse::cell_at(pos.x, pos.y, &rects, r.cell_w as f64, r.cell_h as f64)?;
        let rect = rects.get(hit.pane).copied()?;
        let rows = ((rect.3 as f64 / r.cell_h as f64).floor() as usize).max(1);
        let pane_id = self.tabs.get(self.active)?.panes.get(hit.pane)?.id;
        let row = self.signed_row(pane_id, pos.y - rect.1 as f64, r.cell_h as f64, rows);
        let geom_row = if row < 0 {
            scroll::PEEK_ROW
        } else {
            row as usize
        };
        if let Some((col, side)) = r.hit_cell(rect, geom_row, pos.x) {
            return Some((hit.pane, row, col, side));
        }
        let side = if hit.left { Side::Left } else { Side::Right };
        Some((hit.pane, row, hit.col, side))
    }

    fn mode_of(&self, id: u64) -> TermMode {
        let Some((ti, pi)) = self.find_pane(id) else {
            return TermMode::empty();
        };
        *self.tabs[ti].panes[pi].session.term.lock().mode()
    }

    fn write_pane(&mut self, id: u64, bytes: &[u8]) {
        let Some((ti, pi)) = self.find_pane(id) else {
            return;
        };
        self.tabs[ti].panes[pi].session.write(bytes);
    }

    fn mods_now(&self) -> mouse::Mods {
        mouse::Mods {
            shift: self.modifiers.shift_key(),
            alt: self.modifiers.alt_key(),
            ctrl: self.modifiers.control_key(),
        }
    }

    fn emit_mouse(&mut self, outs: impl IntoIterator<Item = mouse::Out>) {
        for out in outs {
            self.write_pane(out.pane, &out.bytes);
        }
    }

    /// `Window::has_focus`, not the `focused` flag. That flag tracks
    /// Focused events and, on the security branch, starts false — CSI I
    /// at startup has to follow the window itself.
    fn window_focused(&self) -> bool {
        self.window.as_ref().is_some_and(|w| w.has_focus())
    }

    fn focus_reporting(&self, id: u64) -> bool {
        self.mode_of(id).contains(TermMode::FOCUS_IN_OUT)
    }

    /// CSI I / CSI O when the focused pane, or the window, changes. A
    /// program that just enabled 1004 hears the current state on the next
    /// wakeup — xterm sends it as the mode turns on, and the chunk that
    /// carried the mode is what wakes us.
    ///
    /// `focused` is the `Focused` event when one just arrived. Other
    /// callers pass `None`, and the window's `has_focus` answers, so a
    /// window that is already focused at startup still sends CSI I.
    /// The event is not checked again against the window: `has_focus`
    /// can stay true for a moment after `Focused(false)` and would
    /// swallow CSI O.
    fn sync_focus(&mut self, focused: Option<bool>) {
        let window_focused = focused.unwrap_or_else(|| self.window_focused());
        let focused_pane = if window_focused {
            self.tabs
                .get(self.active)
                .and_then(|t| t.panes.get(t.focused).map(|p| p.id))
        } else {
            None
        };
        let old = self.mouse.focus_sent();
        let old_reports = old.is_some_and(|id| self.focus_reporting(id));
        let new_reports = focused_pane.is_some_and(|id| self.focus_reporting(id));
        let outs = self
            .mouse
            .apply_focus(window_focused, focused_pane, new_reports, old_reports);
        self.emit_mouse(outs);
    }

    /// Release a reported button if the program is still tracking, then
    /// drop the grab and the local selection drag. Focus loss, a tab
    /// switch, a new tab, and a pane close all go through here —
    /// otherwise the button stays down in vim after Alt+Tab.
    fn abandon_mouse(&mut self) {
        let proto = self
            .mouse
            .grabbed_pane()
            .map(|id| mouse::Proto::from_mode(self.mode_of(id)))
            .unwrap_or_default();
        let mods = self.mods_now();
        let outs = self.mouse.abandon(proto, mods);
        self.emit_mouse(outs);
        self.clear_link_hover();
    }

    fn focus_pane_at(&mut self, pi: usize) {
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        if pi < t.panes.len() && t.focused != pi {
            t.focused = pi;
            self.request_redraw();
        }
    }

    /// Viewport cell under `pos`, with a bidi row's column mapped back to
    /// the grid cell the program stored. `row` is the offset-aware viewport
    /// row (`-1` is the history line above the screen). `hit` still names
    /// the pane and the unshifted column.
    fn finish_locate(
        &mut self,
        hit: mouse::CellHit,
        row: i32,
        pos: PhysicalPosition<f64>,
        rects: &[render::Rect],
    ) -> Option<(u64, i32, usize)> {
        // copy what the locks need, then drop the pane borrow so the term
        // lock below can re-borrow the tab
        let (pane_id, running, bidi_state, evicted, claude_from, pane_mode, tracked_alt) = {
            let pane = self.tabs.get(self.active)?.panes.get(hit.pane)?;
            let m = pane.session.meta.lock_or_recover();
            (
                pane.id,
                m.running_cmd.clone(),
                m.bidi.clone(),
                m.evicted,
                m.claude_from_abs,
                m.bidi_reorder,
                m.alt_track.on(),
            )
        };
        let (auto, manual) = (self.auto_follow, self.claude_manual);
        let honor = self.config.honor_bdsm();
        let mode = pane_mode.unwrap_or_else(|| self.config.bidi_mode());
        let geom_row = if row < 0 {
            scroll::PEEK_ROW
        } else {
            row as usize
        };
        if row < 0 {
            let (cols, alt, history, off, cells) = {
                let t = self.tabs[self.active].panes[hit.pane].session.term.lock();
                let off = t.grid().display_offset() as u64;
                (
                    t.columns().max(1),
                    mode.effective_alt(t.mode().contains(TermMode::ALT_SCREEN), tracked_alt),
                    t.grid().history_size() as u64,
                    off,
                    render::snapshot_grid_line(&t, scroll::grid_line(row, off as i32)),
                )
            };
            // Same gate as a fully visible row: explicit BDSM (`CSI 8 l`),
            // and the `off` and `alt_off` modes, do not reorder, so the
            // column is the grid's.
            let (reorder, use_stamps) =
                bidi_state.presentation(mode, honor, !running.is_empty(), alt);
            let abs = bidi_ctl::peek_line_abs(evicted, history, off);
            let mut claude = reorder
                && if !auto {
                    manual
                } else {
                    bidi::cmd_is_claude(&running)
                };
            if !alt && auto {
                claude &= claude_from.is_none_or(|start| abs >= start);
            }
            // The painter's direction for this row: its SCP stamp, else
            // its own first strong letter.
            let stamped = if use_stamps {
                bidi_state.direction_at(abs)
            } else {
                bidi_ctl::ParagraphDir::Default
            };
            let dir = if stamped == bidi_ctl::ParagraphDir::Default {
                render::snap_paragraph_dir(&cells)
            } else {
                stamped
            };
            // Same split as draw_pane: TUI fields and the alternate screen
            // are not flush-right.
            let align_edge = reorder && !alt && !((claude || alt) && render::snap_is_tui(&cells));
            let shaped = rects.get(hit.pane).and_then(|rect| {
                self.renderer
                    .as_mut()
                    .and_then(|r| {
                        r.warm_row_ink(*rect, geom_row);
                        r.hit_cell(*rect, geom_row, pos.x)
                    })
                    .map(|h| h.0)
            });
            let shaped = shaped.or_else(|| {
                if !reorder || !cells.iter().any(|c| render::is_rtl(c.c)) {
                    return None;
                }
                let rect = rects.get(hit.pane).copied().unwrap_or((0, 0, 1, 1));
                let x = (pos.x - rect.0 as f64) as f32;
                self.renderer.as_mut().and_then(|r| {
                    r.logical_column_for(&cells, x, rect.2, claude, dir, cols, align_edge)
                })
            });
            let col = scroll::history_click_column(reorder, shaped, hit.col.min(cols - 1));
            return Some((pane_id, row, col.min(cols - 1)));
        }
        let (cols, row, cells, claude, vis, reorder, dir, align_edge, bidi_row) = {
            let t = self.tabs[self.active].panes[hit.pane].session.term.lock();
            let cols = t.columns().max(1);
            let rows = t.screen_lines().max(1);
            let row = row.min(rows as i32 - 1);
            // Explicit BDSM draws Arabic on the grid, so the click column
            // is x / cell_w. Implicit mode uses the same plan as the painter,
            // SCP mark included. Neither applies once the command has ended
            // and the pane is back on the primary screen. `off` and
            // `alt_off` on the alternate screen take the grid path too.
            let term_alt = t.mode().contains(TermMode::ALT_SCREEN);
            let alt = mode.effective_alt(term_alt, tracked_alt);
            let (reorder, use_stamps) =
                bidi_state.presentation(mode, honor, !running.is_empty(), alt);
            let mut claude = reorder
                && if !auto {
                    manual
                } else {
                    bidi::cmd_is_claude(&running)
                };
            let history = t.grid().history_size() as u64;
            let off = t.grid().display_offset() as u64;
            if !alt && auto {
                let abs = bidi_ctl::viewport_line_abs(evicted, history, off, row as u64);
                claude &= claude_from.is_none_or(|start| abs >= start);
            }
            // The row's direction and path as draw_pane picks them: SCP
            // stamp, then the wrapped paragraph's direction, then its own
            // first strong letter. A wrapped RTL tail is bidi without Arabic.
            let (dir, bidi_row) = render::viewport_row_layout(&t, row as usize, |li| {
                if use_stamps {
                    bidi_state.direction_at(bidi_ctl::viewport_line_abs(
                        evicted, history, off, li as u64,
                    ))
                } else {
                    bidi_ctl::ParagraphDir::Default
                }
            });
            let cells = render::snapshot_row(&t, row as usize);
            // Same split as draw_pane: TUI fields and the alternate screen
            // are not flush-right, so a click must not assume they are.
            let align_edge = reorder && !alt && !((claude || alt) && render::snap_is_tui(&cells));
            (
                cols,
                row,
                cells,
                claude,
                hit.col.min(cols - 1),
                reorder,
                dir,
                align_edge,
                bidi_row,
            )
        };
        let mapped = rects
            .get(hit.pane)
            .and_then(|rect| {
                self.renderer.as_mut().and_then(|r| {
                    // Ink for the row is measured here, on the click, not
                    // while the frame was painted.
                    r.warm_row_ink(*rect, row as usize);
                    r.hit_cell(*rect, row as usize, pos.x)
                })
            })
            .map(|h| h.0);
        let col = if let Some(col) = mapped {
            col
        } else if reorder && bidi_row {
            let rect = rects.get(hit.pane).copied().unwrap_or((0, 0, 1, 1));
            let x = (pos.x - rect.0 as f64) as f32;
            self.renderer
                .as_mut()
                .and_then(|r| {
                    r.logical_column_for(&cells, x, rect.2, claude, dir, cols, align_edge)
                })
                .unwrap_or(vis)
        } else {
            vis
        };
        Some((pane_id, row, col.min(cols - 1)))
    }

    /// The cell under the pointer, or None when it isn't over a pane.
    /// The row is offset-aware and may be `-1`.
    fn locate(&mut self, pos: PhysicalPosition<f64>) -> Option<(usize, u64, i32, usize)> {
        let rects = self.pane_rects();
        let (cw, ch) = {
            let r = self.renderer.as_ref()?;
            (r.cell_w as f64, r.cell_h as f64)
        };
        let hit = mouse::cell_at(pos.x, pos.y, &rects, cw, ch)?;
        let pane = hit.pane;
        let rect = *rects.get(pane)?;
        let rows = ((rect.3 as f64 / ch).floor() as usize).max(1);
        let pane_id = self.tabs.get(self.active)?.panes.get(pane)?.id;
        let row = self.signed_row(pane_id, pos.y - rect.1 as f64, ch, rows);
        let (id, row, col) = self.finish_locate(hit, row, pos, &rects)?;
        Some((pane, id, row, col))
    }

    /// Same mapping, but clamped into `pane_id`'s rect so a drag that
    /// leaves the pane still reports a cell of that pane.
    fn cell_in_pane_clamped(
        &mut self,
        pane_id: u64,
        pos: PhysicalPosition<f64>,
    ) -> Option<(i32, usize)> {
        let (ti, pi) = self.find_pane(pane_id)?;
        if ti != self.active {
            return None;
        }
        let rects = self.pane_rects();
        let rect = *rects.get(pi)?;
        let (x, y, w, h) = rect;
        if w <= 0 || h <= 0 {
            return None;
        }
        let (cw, ch) = {
            let r = self.renderer.as_ref()?;
            (r.cell_w as f64, r.cell_h as f64)
        };
        let cx = pos.x.clamp(x as f64, (x + w - 1) as f64);
        let cy = pos.y.clamp(y as f64, (y + h - 1) as f64);
        let hit = mouse::cell_at(cx, cy, &[rect], cw, ch)?;
        // cell_at's pane index is 0 for the one-rect slice; the real index
        // is `pi`, which finish_locate uses to read the grid.
        let hit = mouse::CellHit { pane: pi, ..hit };
        let rows = ((h as f64 / ch).floor() as usize).max(1);
        let row = self.signed_row(pane_id, cy - y as f64, ch, rows);
        let (_, row, col) = self.finish_locate(hit, row, PhysicalPosition::new(cx, cy), &rects)?;
        Some((row, col))
    }

    /// A confirm card is up: the paste/drop/plugin card or the link card.
    pub(crate) fn card_open(&self) -> bool {
        self.pending_paste.is_some() || self.blocked_link.is_some()
    }

    fn overlay_blocks_mouse(&self) -> bool {
        self.shortcuts.is_some()
            || self.settings.is_some()
            || self.cockpit
            || self.palette.is_some()
            || self.posh.is_some()
            || self.pending_close.is_some()
            // PendingPaste is the confirm card. The refused-link card is the
            // same kind of overlay: a Ctrl+click under either must not open.
            || self.pending_paste.is_some()
            || self.blocked_link.is_some()
    }

    fn clear_link_hover(&mut self) {
        if self.link_hover.take().is_some() {
            self.request_redraw();
        }
    }

    /// Working directory for a relative file link. OSC 9;9 fills it today;
    /// OSC 7 (PR5) writes the same field and this is the only reader.
    fn pane_link_cwd(&self, id: u64) -> Option<String> {
        let (ti, pi) = self.find_pane(id)?;
        let raw = self.tabs[ti].panes[pi]
            .session
            .meta
            .lock_or_recover()
            .cwd
            .clone();
        link::cwd_hook(raw.as_deref())
    }

    fn links_on(&mut self, id: u64, row: i32) -> Vec<link::LinkSpan> {
        let key = if row < 0 {
            scroll::PEEK_ROW
        } else {
            row as usize
        };
        if let Some(spans) = self.link_cache.get(id, key) {
            return spans;
        }
        let Some((ti, pi)) = self.find_pane(id) else {
            return Vec::new();
        };
        let spans = {
            let term = self.tabs[ti].panes[pi].session.term.lock();
            link::links_on_viewport(&term, row)
        };
        self.link_cache.insert(id, key, spans.clone());
        spans
    }

    fn invalidate_focused_links(&mut self) {
        if let Some(id) = self.focused_pane_id() {
            self.link_cache.invalidate_pane(id);
        }
    }

    fn focused_pane_id(&self) -> Option<u64> {
        let t = self.tabs.get(self.active)?;
        Some(t.panes.get(t.focused)?.id)
    }

    /// Recompute the hover from `pos`. Returns the logical cell so mouse
    /// motion can be reported without a second hit-test. `None` when the
    /// pointer is not over a pane.
    fn refresh_link_hover(&mut self, pos: PhysicalPosition<f64>) -> Option<(u64, i32, usize)> {
        if self.overlay_blocks_mouse() {
            self.clear_link_hover();
            return None;
        }
        let Some((pane, id, row, col)) = self.locate(pos) else {
            self.clear_link_hover();
            return None;
        };
        let _ = pane;
        let spans = self.links_on(id, row);
        if let Some(span) = spans.into_iter().find(|s| s.contains(col)) {
            let cwd = self.pane_link_cwd(id);
            let next = LinkHover {
                pane_id: id,
                row,
                start_col: span.start_col,
                end_col: span.end_col,
                target: span.tooltip(cwd.as_deref()),
            };
            if self.link_hover.as_ref() != Some(&next) {
                self.link_hover = Some(next);
                self.request_redraw();
            }
        } else {
            self.clear_link_hover();
        }
        Some((id, row, col))
    }

    /// Ctrl+click. True when the cell is a link, whether or not it opened:
    /// the program does not also hear the click, and it does not start a
    /// selection. A refused target raises the card instead.
    fn activate_link_at(&mut self, pos: PhysicalPosition<f64>) -> bool {
        if self.overlay_blocks_mouse() {
            return false;
        }
        let Some((pane, id, row, col)) = self.locate(pos) else {
            return false;
        };
        let spans = self.links_on(id, row);
        let Some(span) = spans.into_iter().find(|s| s.contains(col)) else {
            return false;
        };
        self.focus_pane_at(pane);
        self.sync_focus(None);
        let cwd = self.pane_link_cwd(id);
        // `exists` and `find_code_exe` run on the link thread. A disconnected
        // network drive must not stall this handler.
        match span.activate(
            cwd.as_deref(),
            elevate::configured_editor(self.elevated, self.config.editor.as_deref()),
        ) {
            action @ (link::Action::Browse(_)
            | link::Action::OpenPath(_)
            | link::Action::Reveal(_)
            | link::Action::Edit { .. }
            | link::Action::EditDefault { .. }) => self.spawn_link_open(action),
            link::Action::Confirm { uri, host } => {
                self.blocked_link = Some(BlockedLink {
                    reason: link::host_confirm().to_string(),
                    shown: link::confirm_card_shown(&host, &uri),
                    confirm: Some(link::confirm_open_from_uri(&uri)),
                    is_app_error: false,
                });
                self.clear_link_hover();
                self.request_redraw();
            }
            link::Action::Refuse { reason, shown } => self.refuse_link(reason, &shown),
        }
        true
    }

    /// Browser, association-open, editor, and explorer. A network path must
    /// not stall the window, so the call happens on `bayan-link`.
    fn spawn_link_open(&mut self, action: link::Action) {
        if !link::runs_off_ui_thread(&action) {
            return;
        }
        let shown = match &action {
            link::Action::Browse(s) | link::Action::OpenPath(s) | link::Action::Reveal(s) => {
                s.clone()
            }
            link::Action::Edit { argv, .. } => argv.join(" "),
            link::Action::EditDefault { path, line, col } => match col {
                Some(c) => format!("{path}:{line}:{c}"),
                None => format!("{path}:{line}"),
            },
            link::Action::Confirm { .. } | link::Action::Refuse { .. } => return,
        };
        let proxy = self.proxy.clone();
        let shown_for_fail = shown.clone();
        let elevated = self.elevated;
        let spawned = std::thread::Builder::new()
            .name("bayan-link".into())
            .spawn(move || {
                link::init_link_thread();
                let action = link::settle_live(action, elevated);
                if let link::Action::Confirm { uri, host } = &action {
                    if link::is_browser_uri(uri) {
                        if !link::perform_open(&link::Action::Browse(uri.clone())) {
                            let _ = proxy.send_event(UserEvent::LinkFailed {
                                reason: link::RefuseReason::Editor.label().to_string(),
                                shown: uri.clone(),
                            });
                        }
                    } else {
                        let _ = proxy.send_event(UserEvent::LinkConfirm { path: host.clone() });
                    }
                    return;
                }
                if let link::Action::Refuse { reason, shown } = &action {
                    let _ = proxy.send_event(UserEvent::LinkFailed {
                        reason: reason.label().to_string(),
                        shown: shown.clone(),
                    });
                    return;
                }
                if !link::perform_open(&action) {
                    let _ = proxy.send_event(UserEvent::LinkFailed {
                        reason: link::RefuseReason::Editor.label().to_string(),
                        shown: shown_for_fail,
                    });
                }
            });
        if spawned.is_err() {
            self.refuse_link(link::RefuseReason::Editor, &shown);
        }
    }

    /// Explorer `/select` for a reveal-only card. This does not call
    /// `settle`: an allowlisted extension would become an association open.
    fn spawn_reveal_only(&mut self, path: &str) {
        let path = path.to_string();
        let proxy = self.proxy.clone();
        let owned = path.clone();
        let spawned = std::thread::Builder::new()
            .name("bayan-link".into())
            .spawn(move || {
                link::init_link_thread();
                if link::spawn_reveal(&owned).is_err() {
                    let _ = proxy.send_event(UserEvent::LinkFailed {
                        reason: link::RefuseReason::Editor.label().to_string(),
                        shown: owned,
                    });
                }
            });
        if spawned.is_err() {
            self.refuse_link(link::RefuseReason::Editor, &path);
        }
    }

    fn refuse_link(&mut self, reason: link::RefuseReason, shown: &str) {
        self.blocked_link = Some(BlockedLink {
            reason: reason.label().to_string(),
            shown: link::preview(shown, 160),
            confirm: None,
            is_app_error: false,
        });
        self.clear_link_hover();
        self.request_redraw();
    }

    /// Who owns this left click. The cell is the one [`Self::locate`]
    /// already mapped through `column_for_click`, which is the same cell
    /// [`mouse::Route::press`] would report. An overlay short-circuits
    /// before that: the link under a card is not opened.
    fn left_click_owner(&mut self) -> link::ClickOwner {
        let shift = self.modifiers.shift_key();
        let ctrl = self.modifiers.control_key();
        if self.overlay_blocks_mouse() {
            return link::click_owner(true, false, shift, ctrl, false);
        }
        let Some((_, id, row, col)) = self.locate(self.cursor_pos) else {
            return link::click_owner(false, false, shift, ctrl, false);
        };
        let tracking = mouse::Proto::from_mode(self.mode_of(id)).tracking();
        let on_link = self.links_on(id, row).iter().any(|s| s.contains(col));
        link::click_owner(false, tracking, shift, ctrl, on_link)
    }

    /// What this right click does, and the pane under the pointer. `None`
    /// off the panes: the tab bar and the padding neither copy nor paste.
    fn right_click_owner(&mut self) -> Option<(usize, mouse::RightClick)> {
        let overlay = self.overlay_blocks_mouse();
        let (pane, id, _, _) = self.locate(self.cursor_pos)?;
        let tracking = mouse::Proto::from_mode(self.mode_of(id)).tracking();
        let shift = self.modifiers.shift_key();
        let sel = self.has_selection();
        let cos = self.config.copy_on_select_on();
        let decision = mouse::right_click(overlay, tracking, shift, sel, cos);
        Some((
            pane,
            mouse::with_menu(decision, self.config.right_click_menu_on()),
        ))
    }

    /// Press in a tracking mode, and not Shift (Shift keeps the gesture
    /// for local selection). Focus moves first so the program sees CSI I
    /// before the click.
    fn try_report_press(&mut self, button: mouse::Btn) -> bool {
        if self.overlay_blocks_mouse() {
            return false;
        }
        let Some((pane, id, row, col)) = self.locate(self.cursor_pos) else {
            return false;
        };
        let Some(row) = scroll::screen_row(row) else {
            return false;
        };
        let proto = mouse::Proto::from_mode(self.mode_of(id));
        let cell = mouse::Cell { pane: id, row, col };
        let Some(bytes) = self.mouse.press(cell, button, proto, self.mods_now()) else {
            return false;
        };
        self.focus_pane_at(pane);
        self.sync_focus(None);
        if let Some(out) = bytes {
            self.write_pane(out.pane, &out.bytes);
        }
        if let Some((ti, pi)) = self.find_pane(id) {
            self.tabs[ti].panes[pi].session.term.lock().selection = None;
        }
        true
    }

    /// Release of a button we reported. True when the gesture was ours, so
    /// the selection path must not also copy. Tracking that turned off
    /// still consumes the button and writes nothing.
    fn release_mouse_report(&mut self, button: mouse::Btn) -> bool {
        let Some(pane_id) = self.mouse.grabbed_pane() else {
            return false;
        };
        let proto = mouse::Proto::from_mode(self.mode_of(pane_id));
        let cell = self
            .cell_in_pane_clamped(pane_id, self.cursor_pos)
            .and_then(|(row, col)| {
                scroll::screen_row(row).map(|row| mouse::Cell {
                    pane: pane_id,
                    row,
                    col,
                })
            });
        match self.mouse.release(button, cell, proto, self.mods_now()) {
            mouse::Take::Ignore => false,
            mouse::Take::Consumed(bytes) => {
                if let Some(out) = bytes {
                    self.write_pane(out.pane, &out.bytes);
                }
                true
            }
        }
    }

    fn report_drag(&mut self, pos: PhysicalPosition<f64>) {
        if self.overlay_blocks_mouse() {
            return;
        }
        let Some(pane_id) = self.mouse.grabbed_pane() else {
            return;
        };
        let proto = mouse::Proto::from_mode(self.mode_of(pane_id));
        let Some((row, col)) = self.cell_in_pane_clamped(pane_id, pos) else {
            return;
        };
        let Some(row) = scroll::screen_row(row) else {
            return;
        };
        let cell = mouse::Cell {
            pane: pane_id,
            row,
            col,
        };
        if let Some(out) = self.mouse.drag(cell, proto, self.mods_now()) {
            self.write_pane(out.pane, &out.bytes);
        }
    }

    fn report_motion_at(&mut self, id: u64, row: usize, col: usize) {
        if self.overlay_blocks_mouse() || self.modifiers.shift_key() {
            return;
        }
        let proto = mouse::Proto::from_mode(self.mode_of(id));
        let cell = mouse::Cell { pane: id, row, col };
        if let Some(out) = self.mouse.motion(cell, proto, self.mods_now()) {
            self.write_pane(out.pane, &out.bytes);
        }
    }

    fn wheel_view(&self, id: u64) -> mouse::View {
        let mode = self.mode_of(id);
        mouse::View {
            id,
            proto: mouse::Proto::from_mode(mode),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
        }
    }

    /// Fire a toast for something the PROGRAM asked for. Rate-limited:
    /// the trigger is untrusted output, so it must not be able to spam.
    fn toast_from_pty(&mut self, title: &str, body: &str) {
        let now = Instant::now();
        if !toast_due(self.last_toast, now, TOAST_MIN_GAP) {
            return;
        }
        if let Some(h) = self.hwnd() {
            self.last_toast = Some(now);
            toast::show(h, title, body);
        }
    }

    fn has_selection(&self) -> bool {
        self.session().is_some_and(|s| {
            s.term
                .lock()
                .selection
                .as_ref()
                .is_some_and(|sel| !sel.is_empty())
        })
    }

    /// Copy the current selection to the system clipboard; optionally clear it.
    ///
    /// `copy.clean` decides whether the TUI margin, trailing spaces and soft
    /// wraps come off. Copy-mode yanks come through here too, so the same
    /// setting covers `y`.
    fn copy_selection(&mut self, clear: bool) -> bool {
        self.copy_clipboard(clear, false)
    }

    /// Copy the selection cleaned, whatever `copy.clean` says.
    fn copy_selection_clean(&mut self) -> bool {
        self.copy_clipboard(false, true)
    }

    fn copy_clipboard(&mut self, clear: bool, force_clean: bool) -> bool {
        let Some(session) = self.session() else {
            return false;
        };
        let text = {
            let mut t = session.term.lock();
            let txt = t.selection_to_string();
            let rows = copyclean::selection_rows(&t);
            if clear {
                t.selection = None;
            }
            txt.map(|raw| (raw, rows))
        };
        // claude_active locks the term too; take it only after the guard above
        let claude = self.claude_active();
        let clean = force_clean || self.config.clean_copy().enabled(claude);
        match text {
            Some((raw, rows)) if !raw.is_empty() => {
                // Claude rows hold VISUAL-order Arabic: restore logical so a
                // paste elsewhere reads correctly (M7 gap closed). Clean copy
                // runs that same restore; it does not reorder by visual x.
                let s = copyclean::clipboard_text(&raw, &rows, clean, claude);
                if s.is_empty() {
                    return false;
                }
                self.set_clipboard(s);
                true
            }
            _ => false,
        }
    }

    /// The system clipboard, opened on first use. A failed open is retried
    /// next time rather than remembered.
    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()
    }

    fn set_clipboard(&mut self, s: String) {
        if let Some(c) = self.clipboard() {
            let _ = c.set_text(s);
        }
    }

    /// Indices of the active tab's panes that also receive typed input:
    /// every pane but the focused one while broadcast is on, else none.
    fn broadcast_targets(&self) -> Vec<usize> {
        self.tabs
            .get(self.active)
            .map(|t| broadcast_targets(t.broadcast, t.panes.len(), t.focused))
            .unwrap_or_default()
    }

    /// Ctrl+Shift+F3. Cycles the focused pane only: full → alt screen off → off.
    /// The config stays the default for panes that have not been toggled.
    fn toggle_bidi_reorder(&mut self) {
        let config_mode = self.config.bidi_mode();
        let Some(session) = self.session_mut() else {
            return;
        };
        let mut meta = session.meta.lock_or_recover();
        let next = meta.bidi_reorder.unwrap_or(config_mode).cycle();
        meta.bidi_reorder = Some(next);
        drop(meta);
        self.request_redraw();
    }

    /// The focused pane's effective BiDi mode: its toggle, or the config.
    fn focused_bidi_mode(&self) -> bidi_ctl::BidiReorder {
        let stored = self
            .session()
            .map(|s| s.meta.lock_or_recover().bidi_reorder);
        stored.flatten().unwrap_or_else(|| self.config.bidi_mode())
    }

    /// Ctrl+Shift+B. A tab with one pane has nobody to broadcast to.
    fn toggle_broadcast(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.broadcast = !t.broadcast && t.panes.len() > 1;
        }
        self.request_redraw();
    }

    /// Ctrl+Shift+Z: the focused pane fills the tab, or the split comes back.
    fn toggle_zoom(&mut self) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.zoomed = !t.zoomed && t.panes.len() > 1;
        }
        // a drag in progress was measured against the old rects
        self.abandon_mouse();
        self.drag_divider = None;
        self.relayout_active();
        self.request_redraw();
    }

    /// The right-click menu's rows, for the active tab as it is now: copy
    /// only with a selection, the pane rows only with a split, and the
    /// zoom and broadcast rows saying which way they will flip.
    fn context_rows(&self) -> Vec<(PaletteRow, String)> {
        let (panes, zoomed, broadcast) =
            self.tabs.get(self.active).map_or((1, false, false), |t| {
                (t.panes.len(), t.zoomed, t.broadcast)
            });
        let mut rows = context_menu_rows(self.has_selection(), panes, zoomed, broadcast);
        if !self.elevated {
            rows.push((PaletteRow::OpenSsh, ssh::label_open().to_string()));
            if self.config.mux_enabled() {
                rows.push((PaletteRow::OpenDetached, mux::label_reattach().to_string()));
                if self.focused_is_mux() {
                    rows.push((PaletteRow::DetachSession, mux::label_detach().to_string()));
                }
            }
        }
        rows
    }

    /// Right-click: the menu opens under the pointer, on the pane that was
    /// clicked. It is the palette in menu mode, so the keyboard, the wheel
    /// and Esc already work on it.
    fn open_context_menu(&mut self) {
        let at = (self.cursor_pos.x, self.cursor_pos.y);
        self.open_palette(String::new());
        if let Some(p) = self.palette.as_mut() {
            p.menu = Some(at);
        }
    }

    /// Right-click on the tab bar, or the pane menu's «تبويب جديد بطرفية…»:
    /// the installed shells under the pointer. A row opens a new tab with
    /// that shell in the group on screen, through the same `PaletteRow::Shell`
    /// path the palette's «تبويب جديد — …» rows take.
    fn open_shell_menu(&mut self) {
        let at = (self.cursor_pos.x, self.cursor_pos.y);
        self.open_shell_picker(false, at);
    }

    /// The tab-bar arrow, Alt+Down, or Enter while the arrow is focused.
    fn open_shell_from_arrow(&mut self) {
        let at = self.shell_arrow_anchor();
        self.shell_arrow_focused = true;
        self.open_shell_picker(true, at);
    }

    fn open_shell_picker(&mut self, arrow: bool, at: (f64, f64)) {
        let menu = self.shell_menu(arrow);
        shellmenu::log_skips(&menu.skipped);
        self.open_palette(String::new());
        if let Some(p) = self.palette.as_mut() {
            p.menu = Some(at);
            p.shells_menu = true;
            p.shell_arrow = arrow;
        }
        self.request_redraw();
    }

    /// Installed shells, plus any profile `shells::find_profile` accepts.
    /// Elevated windows pass the bit from `elevate::process_is_elevated`
    /// and do not list SSH or the mux.
    fn shell_menu(&self, arrow: bool) -> shellmenu::Menu {
        let default = if self.elevated {
            shells::elevated_default_command(&self.shell_profiles)
        } else {
            self.config.shell_program()
        };
        // Detected shells are the profiles (`shells::detect` / `-p`).
        // An extra record that is not one of them is skipped inside `build`.
        shellmenu::build(
            &self.shell_profiles,
            &[],
            &default,
            self.elevated,
            shellmenu::Offers {
                ssh: arrow,
                mux: arrow && self.config.mux_enabled(),
            },
        )
    }

    fn shell_picker_rows(&self) -> Vec<shellmenu::Row> {
        let arrow = self.palette.as_ref().is_some_and(|p| p.shell_arrow);
        self.shell_menu(arrow).rows
    }

    /// The point the menu hangs from: the arrow's bottom corner on the
    /// side the menu opens toward.
    fn shell_arrow_anchor(&self) -> (f64, f64) {
        let Some((r, w)) = self.renderer.as_ref().zip(self.window.as_ref()) else {
            return (self.cursor_pos.x, self.cursor_pos.y);
        };
        let pw = w.inner_size().width as usize;
        let (labels, active) = self.visible_labels();
        let titles: Vec<&str> = labels.iter().map(String::as_str).collect();
        let Some((x, y, bw, bh)) = r.shell_arrow_rect(pw, &titles, active) else {
            return (self.cursor_pos.x, self.cursor_pos.y);
        };
        let ax = if lang::rtl() { x + bw } else { x };
        (ax as f64, (y + bh) as f64)
    }

    /// Ctrl+Shift+R: the palette, in naming mode. Whatever is typed becomes
    /// the tab's name on Enter; the first row restores the folder name.
    fn open_rename_tab(&mut self) {
        let current = self
            .tabs
            .get(self.active)
            .and_then(|t| t.name.clone())
            .unwrap_or_default();
        self.open_palette(current);
        if let Some(p) = self.palette.as_mut() {
            p.rename = Some(RenameTarget::Tab);
        }
    }

    fn rename_tab(&mut self, name: Option<String>) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.name = name.as_deref().and_then(clean_tab_name);
        }
        self.save_state();
        self.request_redraw();
    }

    /// Ctrl+Shift+L: select and copy what the last finished command
    /// printed, read between its OSC 133 marks. The selection is the
    /// feedback: the copied block stays highlighted. It goes through
    /// `copy_selection`, so Claude rows are restored to logical order the
    /// same way a mouse copy is. Nothing happens without shell integration
    /// (cmd, or a shell Bayan did not wrap).
    fn copy_last_output(&mut self) {
        let Some(s) = self.session() else {
            return;
        };
        let range = {
            let m = s.meta.lock_or_recover();
            term::last_output_range(&m.marks).map(|r| (r, m.evicted))
        };
        let Some(((start, end), evicted)) = range else {
            return;
        };
        {
            let mut t = s.term.lock();
            let history = t.grid().history_size() as u64;
            let Some((first, last)) = output_lines(start, end, evicted, history, t.screen_lines())
            else {
                return;
            };
            let mut sel = Selection::new(
                SelectionType::Lines,
                Point::new(Line(first), Column(0)),
                Side::Left,
            );
            sel.update(Point::new(Line(last), Column(0)), Side::Right);
            t.selection = Some(sel);
        }
        self.copy_selection(false);
        self.request_redraw();
    }

    fn paste(&mut self) {
        let Some(txt) = self.clipboard().and_then(|c| c.get_text().ok()) else {
            return;
        };
        if txt.is_empty() {
            return;
        }
        // EasyTer's paste guard: confirm before a multi-line/huge paste.
        // `None` means the text is safe to type.
        self.arm_pending_paste(txt, None);
    }

    /// Hold `txt` for the confirm card. `fallback` is the reason when the
    /// guard itself is quiet — a quoted drop onto vim, or onto a shell that
    /// is not at its prompt. The card draws whenever this is set, including
    /// when the text has no control characters.
    fn arm_pending_paste(&mut self, txt: String, fallback: Option<term::PasteRisk>) {
        self.arm_pending(PendingKind::Paste, txt, fallback);
    }

    /// `arm_pending_paste` for a dropped path (R-01): if it needs a card, the
    /// card sends to the focused pane only; if not, it is typed there now.
    pub(crate) fn arm_pending_drop(&mut self, txt: String, fallback: Option<term::PasteRisk>) {
        self.arm_pending(PendingKind::Drop, txt, fallback);
    }

    fn arm_pending(&mut self, kind: PendingKind, txt: String, fallback: Option<term::PasteRisk>) {
        let (lines, chars, mut reason) = if let Some(g) = term::needs_paste_guard(&txt) {
            (g.lines, g.chars, g.risk.label().to_string())
        } else if let Some(risk) = fallback {
            (1, txt.chars().count(), risk.label().to_string())
        } else {
            match kind {
                PendingKind::Paste => self.send_paste(&txt),
                PendingKind::Drop | PendingKind::Command => self.send_focused(kind, &txt),
                PendingKind::ApproveLua => {}
            }
            return;
        };
        // R-01: say so when Enter will type this into more than one pane
        if kind == PendingKind::Paste {
            if let Some(note) = broadcast_note(self.broadcast_targets().len()) {
                reason.push_str(&note);
            }
        }
        self.pending_paste = Some(PendingPaste {
            kind,
            pane: self.focused_pane_id(),
            text: txt,
            lines,
            chars,
            reason,
            lua: None,
            scroll: 0,
            saw_end: true,
        });
        self.request_redraw();
    }

    /// Enter on the confirm card (R-02): send it where its kind says, but
    /// only if the pane it was raised for is still the focused one.
    pub(crate) fn confirm_pending(&mut self) {
        let Some(p) = self.pending_paste.take() else {
            return;
        };
        if p.kind == PendingKind::ApproveLua {
            self.approve_lua(p);
            self.after_confirm_card();
            self.request_redraw();
            return;
        }
        if !card_still_targets(p.pane, self.focused_pane_id()) {
            self.request_redraw();
            return;
        }
        match p.kind {
            PendingKind::Paste => self.send_paste(&p.text),
            PendingKind::Drop | PendingKind::Command => self.send_focused(p.kind, &p.text),
            PendingKind::ApproveLua => {}
        }
        self.after_confirm_card();
        self.request_redraw();
    }

    /// Esc on the confirm card. A Lua file that was refused stays unapproved
    /// for this session; the next waiting file is raised.
    pub(crate) fn dismiss_paste_card(&mut self) {
        if let Some(hash) = self
            .pending_paste
            .as_ref()
            .and_then(|p| p.lua.as_ref())
            .map(|s| s.hash.clone())
        {
            self.lua_skipped.push(hash);
        }
        self.pending_paste = None;
        self.lua_approve_not_before = None;
        self.after_confirm_card();
        self.request_redraw();
    }

    fn after_confirm_card(&mut self) {
        if self.pending_paste.is_some() {
            return;
        }
        self.show_next_lua_card();
        if self.pending_paste.is_none() {
            self.pump_lua_sends();
        }
    }

    fn show_next_lua_card(&mut self) {
        if self.pending_paste.is_some() {
            return;
        }
        let skipped = self.lua_skipped.clone();
        let Some(index) = self
            .lua_pending
            .iter()
            .position(|p| !skipped.iter().any(|h| h == &p.hash))
        else {
            return;
        };
        let script = self.lua_pending.remove(index);
        self.raise_lua_card(script);
    }

    fn raise_lua_card(&mut self, script: lua_host::PendingScript) {
        let text = script.preview.clone();
        let rows = card_preview(PendingKind::ApproveLua, &text);
        let saw_end = rows.len() <= LUA_CARD_ROWS;
        let mut reason = script.reason();
        if !saw_end {
            reason.push_str(tr(" — مرّر حتى آخر السكربت", " — scroll to the end"));
        }
        let lines = text.lines().count().max(1);
        let chars = text.chars().count();
        self.pending_paste = Some(PendingPaste {
            kind: PendingKind::ApproveLua,
            pane: None,
            text,
            lines,
            chars,
            reason,
            lua: Some(script),
            scroll: 0,
            saw_end,
        });
    }

    /// Wheel and arrows while the approval card is up. Returns whether the
    /// card consumed the input.
    fn scroll_lua_card(&mut self, delta: i32) -> bool {
        let Some(p) = self.pending_paste.as_mut() else {
            return false;
        };
        if p.kind != PendingKind::ApproveLua {
            return false;
        }
        let rows = card_preview(p.kind, &p.text);
        let (_, scroll, _) = lua_card_window(&rows, p.scroll);
        if rows.len() <= LUA_CARD_ROWS {
            p.saw_end = true;
            p.scroll = scroll;
            return true;
        }
        let max = rows.len() - LUA_CARD_ROWS;
        let next = (scroll as i32 + delta).clamp(0, max as i32) as usize;
        p.scroll = next;
        if next >= max {
            p.saw_end = true;
        }
        true
    }

    fn scroll_lua_card_key(&mut self, key: &winit::keyboard::Key) -> bool {
        let lua = self
            .pending_paste
            .as_ref()
            .is_some_and(|p| p.kind == PendingKind::ApproveLua);
        if !lua {
            return false;
        }
        let delta = match key {
            winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowDown) => 1,
            winit::keyboard::Key::Named(winit::keyboard::NamedKey::ArrowUp) => -1,
            winit::keyboard::Key::Named(winit::keyboard::NamedKey::PageDown) => {
                LUA_CARD_ROWS as i32
            }
            winit::keyboard::Key::Named(winit::keyboard::NamedKey::PageUp) => {
                -(LUA_CARD_ROWS as i32)
            }
            _ => return false,
        };
        self.scroll_lua_card(delta);
        true
    }

    fn approve_lua(&mut self, card: PendingPaste) {
        let Some(script) = card.lua.clone() else {
            return;
        };
        if !script.shows_everything() {
            self.lua_approve_not_before = Some(std::time::Instant::now() + LUA_APPROVE_DELAY);
            self.raise_lua_card(script.reveal());
            self.request_redraw();
            return;
        }
        if !lua_enter_ready(self.lua_approve_not_before, std::time::Instant::now()) || !card.saw_end
        {
            self.pending_paste = Some(card);
            self.request_redraw();
            return;
        }
        self.lua_approve_not_before = None;
        match lua_host::unchanged(&script) {
            Ok(()) => {
                lua_host::grant(&mut self.json_config, &script.hash);
                self.save_config();
                let outcome = lua_host::reload(&self.json_config, &config::bayan_dir());
                self.apply_lua_outcome(outcome);
            }
            Err(err) => {
                eprintln!("bayan: {err}");
                self.report_error(
                    tr("تعذر اعتماد السكربت", "Could not approve the script"),
                    "تغيّر الملف ويحتاج اعتمادًا جديدًا.",
                );
                let outcome = lua_host::reload(&self.json_config, &config::bayan_dir());
                self.apply_lua_outcome(outcome);
            }
        }
    }

    fn apply_lua_outcome(&mut self, outcome: lua_host::Outcome) {
        self.json_config = outcome.persisted;
        self.config = outcome.config;
        self.keymap = keybinds::effective_map(&self.config);
        lang::set(self.config.lang());
        self.lua_plugins = outcome.plugins;
        self.lua_pending = outcome.pending;
        self.lua_errors = outcome.shown;
        self.lua_notice = outcome.notice;
        if self.renderer.is_some() && !self.renderer_building {
            self.rebuild_renderer();
        }
        self.apply_opacity();
        self.report_lua_errors();
    }

    fn report_lua_errors(&mut self) {
        if let Some(notice) = self.lua_notice.take() {
            self.report_error(tr("مسؤول", "Administrator"), &notice);
        }
        if self.lua_errors.is_empty() {
            return;
        }
        let detail = self.lua_errors.join("\n");
        self.lua_errors.clear();
        self.report_error(tr("تعذر تشغيل لوا", "Lua did not run"), &detail);
    }

    fn fire_lua_chord(&mut self, chord: &str) -> bool {
        if !self.lua_plugins.has_binding(chord) {
            return false;
        }
        let sends = self.lua_plugins.fire(chord);
        self.lua_send_queue.extend(sends);
        if self.pending_paste.is_none() {
            self.pump_lua_sends();
        }
        self.request_redraw();
        true
    }

    fn pump_lua_sends(&mut self) {
        if self.pending_paste.is_some() {
            return;
        }
        if let Some(send) = self.lua_send_queue.pop_front() {
            let pane = self.focused_pane_id();
            self.pending_paste = Some(plugin_pending(&send.plugin, &send.text, pane));
        }
    }

    fn emit_lua_event(&mut self, event: &str, fields: &[(&str, &str)]) {
        let sends = self.lua_plugins.emit(event, fields);
        if sends.is_empty() {
            return;
        }
        self.lua_send_queue.extend(sends);
        if self.pending_paste.is_none() {
            self.pump_lua_sends();
        }
    }

    /// A drop or a plugin command into the FOCUSED pane only. A drop keeps
    /// the pane's own bracketed-paste mode; a plugin command is typed
    /// unbracketed so its CR runs it.
    fn send_focused(&mut self, kind: PendingKind, txt: &str) {
        let bracketed =
            kind == PendingKind::Drop && self.term_mode().contains(TermMode::BRACKETED_PASTE);
        let body = term::normalize_paste(txt, bracketed);
        self.last_input = Instant::now();
        if let Some(s) = self.session_mut() {
            s.write(body.as_bytes());
        }
    }

    fn send_paste(&mut self, txt: &str) {
        let bracketed = self.term_mode().contains(TermMode::BRACKETED_PASTE);
        let body = term::normalize_paste(txt, bracketed);
        self.last_input = Instant::now(); // pasting is typing, blink-wise
                                          // broadcast: each other pane gets the paste wrapped for its own
                                          // bracketed-paste mode
        for pi in self.broadcast_targets() {
            let session = &mut self.tabs[self.active].panes[pi].session;
            let bracketed = session
                .term
                .lock()
                .mode()
                .contains(TermMode::BRACKETED_PASTE);
            session.write(term::normalize_paste(txt, bracketed).as_bytes());
        }
        if let Some(s) = self.session_mut() {
            s.write(body.as_bytes());
        }
    }

    fn scroll_lines(&mut self, n: i32) {
        if let Some(id) = self.focused_pane_id() {
            self.drop_smooth(id);
        }
        if let Some(s) = self.session() {
            s.term.lock().scroll_display(Scroll::Delta(n));
        }
        self.invalidate_focused_links();
        self.request_redraw();
    }

    /// Find the next search hit. Enter walks UP from the bottom (the match
    /// you can see is the one you want), Shift+Enter walks back down.
    fn search_step(&mut self, dir: Direction) {
        let query = match &self.search {
            Some(st) if !st.query.is_empty() => {
                st.selected_needle.as_ref().unwrap_or(&st.query).clone()
            }
            _ => return,
        };
        let Some(session) = self.session() else {
            return;
        };
        let found = {
            let mut t = session.term.lock();
            let hl = self.search.as_ref().and_then(|s| s.hl.clone());
            let m = find_match(&mut t, &query, &hl, dir);
            if let Some(m) = &m {
                // bring the hit into view, roughly centered
                let cur = t.grid().display_offset() as i32;
                let want = (-(m.start().line.0) + t.screen_lines() as i32 / 2)
                    .clamp(0, t.grid().history_size() as i32);
                t.scroll_display(Scroll::Delta(want - cur));
            }
            m
        };
        if let Some(id) = self.focused_pane_id() {
            self.drop_smooth(id);
        }
        self.invalidate_focused_links();
        if let (Some(st), Some(m)) = (self.search.as_mut(), found) {
            st.hl = Some(m);
        }
        self.request_redraw();
    }

    /// Keyboard handling while the search bar is open.
    fn search_input(&mut self, event: &KeyEvent, shift: bool) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.search = None;
            }
            Key::Named(NamedKey::Enter) => {
                self.search_step(if shift {
                    Direction::Right
                } else {
                    Direction::Left
                });
                return; // search_step already redraws
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(st) = self.search.as_mut() {
                    st.query.pop();
                    st.hl = None;
                    st.selected_needle = None;
                }
            }
            _ => {
                if let Some(t) = event.text.as_ref() {
                    self.search_push(t);
                }
            }
        }
        self.request_redraw();
    }

    /// Typed or IME-committed text into the search query, controls dropped.
    pub(crate) fn search_push(&mut self, t: &str) {
        let Some(st) = self.search.as_mut() else {
            return;
        };
        for ch in t.chars() {
            if !ch.is_control() {
                st.query.push(ch);
                st.hl = None;
                st.selected_needle = None;
            }
        }
    }

    fn redraw(&mut self) {
        // Before a client, this is a flag check. After one, an unchanged
        // stamp does not walk the grid; a flood builds at about 10 Hz.
        self.publish_reader();
        // A scroll under a still pointer moves the link. Refresh only while
        // a hover is showing, and never during a drag: that path must not
        // redraw on every pixel.
        if self.link_hover.is_some() && !self.mouse.selecting && self.mouse.grabbed_pane().is_none()
        {
            let pos = self.cursor_pos;
            let _ = self.refresh_link_hover(pos);
        }
        // the bar's visibility is a per-frame fact (tab count × setting)
        let hide = self.config.hide_single_tab_on() && self.tabs.len() <= 1;
        let chip = self.group_chip();
        if let Some(r) = self.renderer.as_mut() {
            r.set_bar_hidden(hide);
            r.set_group_chip(chip);
        }
        // computed before the gpu/renderer borrows (they read &self broadly)
        let cockpit_entries = if self.cockpit {
            self.cockpit_entries()
        } else {
            Vec::new()
        };
        let settings_open = self.settings.is_some();
        let settings_theme = self.active_theme();
        let settings_size = self.config.font_size.unwrap_or(render::DEFAULT_FONT_SIZE) as i32;
        // the family ACTUALLY in use (post-fallback), not the config wish
        let settings_family = self
            .renderer
            .as_ref()
            .map(|r| r.family().to_string())
            .unwrap_or_default();
        // likewise the Arabic font actually in use, or Auto
        let settings_arabic = self
            .renderer
            .as_ref()
            .and_then(|r| r.arabic_family().map(str::to_string))
            .unwrap_or_else(|| tr("تلقائي", "Auto").to_string());
        let cursor_style = self.config.cursor();
        let (cursor_on, _) = self.cursor_blink_state();
        self.blink_drawn_on = cursor_on;
        let shortcuts_view: Option<(Vec<render::ShortcutRow>, usize, bool, Option<String>)> =
            self.shortcuts.as_ref().map(|st| {
                let rows = self
                    .keymap
                    .iter()
                    .map(|(a, c)| render::ShortcutRow {
                        label: a.label().to_string(),
                        chord: c.display(),
                        custom: *c != a.default_chord(),
                    })
                    .collect();
                (rows, st.sel, st.capturing, st.flash.clone())
            });
        let posh_view: Option<PoshView> = self
            .posh
            .as_ref()
            .map(|p| (p.names.clone(), p.previews.clone(), p.sel));
        // computed before the renderer is borrowed, like close_msg above
        let palette_view: Option<PaletteView> = self.palette.as_ref().map(|p| {
            // every built-in row shows its live shortcut, so the menu and the
            // palette teach the keys (a rebinding shows up here too)
            let rows: Vec<render::MenuRow> = self
                .palette_rows(&p.query)
                .into_iter()
                .map(|(row, label)| render::MenuRow {
                    hint: row_shortcut(&self.keymap, &row),
                    label,
                })
                .collect();
            let sel = p.sel.min(rows.len().saturating_sub(1));
            (p.query.clone(), rows, sel, p.menu)
        });
        let close_msg = self.pending_close.map(|target| self.close_message(target));
        let rects = self.pane_rects();
        let sb_content = self.content_rect();
        let sb_pad = self.padding_physical();
        let sb_width = self.scrollbar_width_px();
        // the bar shows the active group's tabs only
        let tab_infos: Vec<render::TabInfo> = self
            .visible_tabs()
            .0
            .into_iter()
            .map(|i| (i, &self.tabs[i]))
            .map(|(i, t)| render::TabInfo {
                title: t.label(),
                busy: t.busy(i == self.active),
                attention: t.attention,
                active: i == self.active,
                progress: t.progress(),
            })
            .collect();
        let config_notice = self.config_notice.clone();
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let px = window.inner_size();
        if px.width == 0 || px.height == 0 {
            return;
        }
        let mut verts: Vec<gpu::Vertex> = Vec::new();
        let mut sprites: Vec<render::ImageSprite> = Vec::new();
        match (self.renderer.as_mut(), self.tabs.get(self.active)) {
            (Some(renderer), Some(tab)) if !rects.is_empty() => {
                renderer.begin_frame();
                self.bars.begin_frame();
                let bordered = tab.panes.len() > 1 && !tab.zoomed;
                let (auto_follow, claude_manual) = (self.auto_follow, self.claude_manual);
                let mut focused_claude = false;
                let mut focused_copy = false;
                for (pi, (pane, rect)) in tab.panes.iter().zip(&rects).enumerate() {
                    if !rect_visible(rect) {
                        continue; // a pane zoomed away is not drawn
                    }
                    let focused = pi == tab.focused;
                    // meta snapshot BEFORE the term lock (lock order term>meta
                    // only applies when holding both; here they don't overlap)
                    let (
                        marks,
                        running_cmd,
                        bidi_state,
                        evicted,
                        claude_from,
                        pane_mode,
                        tracked_alt,
                    ): PaneSnap = {
                        let m = pane.session.meta.lock_or_recover();
                        (
                            // global -> grid space: subtract the evictions
                            m.marks
                                .iter()
                                .filter(|c| c.abs >= m.evicted)
                                .map(|c| ((c.abs - m.evicted) as usize, c.exit))
                                .collect(),
                            m.running_cmd.clone(),
                            m.bidi.clone(),
                            m.evicted,
                            m.claude_from_abs,
                            m.bidi_reorder,
                            m.alt_track.on(),
                        )
                    };
                    // Cell size is published before the term lock. Taking meta
                    // again while `term` is held deadlocks the reader, which
                    // locks term and then meta.
                    let cw = renderer.cell_w.round().max(1.0) as u32;
                    let ch = renderer.cell_h.round().max(1.0) as u32;
                    let image_snaps = {
                        let mut m = pane.session.meta.lock_or_recover();
                        m.images.set_cell_px(cw, ch);
                        m.images.snapshot()
                    };
                    let t = pane.session.term.lock();
                    // Explicit BDSM suppresses reordering for the whole pane,
                    // Claude restoration included, and only while the command
                    // is running or the alt screen is up. honor_bdsm defaults on.
                    // The Arc is the frame's snapshot: cloning it does not copy
                    // the stamp list. alt_off also counts ?47 and ?1047.
                    let term_alt = t.mode().contains(TermMode::ALT_SCREEN);
                    let mode = pane_mode.unwrap_or_else(|| self.config.bidi_mode());
                    let alt = mode.effective_alt(term_alt, tracked_alt);
                    let (reorder, use_stamps) = bidi_state.presentation(
                        mode,
                        self.config.honor_bdsm(),
                        !running_cmd.is_empty(),
                        alt,
                    );
                    let claude_pane = reorder
                        && if !auto_follow {
                            claude_manual
                        } else {
                            bidi::cmd_is_claude(&running_cmd)
                        };
                    // copy mode: a solid block that does not blink, so the
                    // cursor reads as the selection's, not the shell's
                    let copy_mode = t.mode().contains(TermMode::VI);
                    if focused {
                        focused_claude = claude_pane;
                        focused_copy = copy_mode;
                    }
                    let history = t.grid().history_size() as u64;
                    let off = t.grid().display_offset() as u64;
                    let line_dir: Vec<bidi_ctl::ParagraphDir> = if use_stamps {
                        (0..t.screen_lines())
                            .map(|li| {
                                bidi_state.direction_at(bidi_ctl::viewport_line_abs(
                                    evicted, history, off, li as u64,
                                ))
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let peek_dir = if use_stamps {
                        bidi_state.direction_at(bidi_ctl::peek_line_abs(evicted, history, off))
                    } else {
                        bidi_ctl::ParagraphDir::Default
                    };
                    let (link_hover, link_tip) = match &self.link_hover {
                        Some(h) if h.pane_id == pane.id => {
                            let row = if h.row < 0 {
                                scroll::PEEK_ROW
                            } else {
                                h.row as usize
                            };
                            (Some((row, h.start_col, h.end_col)), Some(&h.target))
                        }
                        _ => (None, None),
                    };
                    let view = render::PaneView {
                        rect: *rect,
                        focused,
                        cursor: if copy_mode {
                            config::CursorStyle::Block
                        } else {
                            cursor_style
                        },
                        cursor_on: cursor_on || copy_mode,
                        bordered,
                        claude: claude_pane,
                        claude_from: if alt || !auto_follow {
                            None
                        } else {
                            claude_from.map(|start| {
                                start as i64 - evicted as i64 - history as i64 + off as i64
                            })
                        },
                        reorder,
                        bidi_mode: mode,
                        line_dir: &line_dir,
                        search_match: if focused {
                            self.search.as_ref().and_then(|s| s.hl.as_ref())
                        } else {
                            None
                        },
                        marks: &marks,
                        link_hover,
                        link_tip,
                        peek_dir,
                        scroll_px: if alt || !self.config.smooth_scroll_on() {
                            0
                        } else {
                            let sub = self.smooth.get(&pane.id).map(|s| s.subline).unwrap_or(0.0);
                            scroll::offset_px(sub, renderer.cell_h)
                        },
                    };
                    // after the view: an image rides the same sub-line
                    // shift as the row it is anchored to
                    sprites.extend(render::image_sprites(
                        *rect,
                        renderer.cell_w,
                        renderer.cell_h,
                        evicted.saturating_add(history),
                        off,
                        view.scroll_px,
                        &image_snaps,
                    ));
                    let rows = t.screen_lines();
                    renderer.draw_pane(&mut verts, &view, &t);
                    // Smooth scroll's sub-line remainder is the fraction's
                    // unit: lines in [0, 1) added to display_offset. Zero on
                    // the alternate screen and with smooth scroll off.
                    let fraction = if view.scroll_px > 0 {
                        self.smooth.get(&pane.id).map_or(0.0, |s| s.subline)
                    } else {
                        0.0
                    };
                    let place = scrollbar::Place::new(history as usize + rows, rows, off as usize)
                        .with_fraction(fraction);
                    let side = self.config.scrollbar.side();
                    let width = sb_width;
                    let autohide = self.config.scrollbar.autohide_on();
                    let room = sb_content
                        .map(|c| scrollbar::room_px(pi, &rects, side, c, sb_pad))
                        .unwrap_or(0);
                    let now = Instant::now();
                    self.bars.observe(pane.id, off as usize, now);
                    let engaged = self.bars.hover == Some(pane.id) || self.bars.dragging(pane.id);
                    if scrollbar::shown(autohide, engaged, self.bars.last_of(pane.id), now) {
                        if let Some(geom) = scrollbar::layout(*rect, side, width, room, place, alt)
                        {
                            // clip to the gutter, so a quad cannot land on a cell
                            renderer.draw_scrollbar(
                                &mut verts, geom.track, geom.track, geom.thumb, engaged,
                            );
                            self.bars.mark_painted(pane.id);
                        }
                    }
                }
                renderer.use_ui_metrics(true);
                renderer.draw_chrome(
                    &mut verts,
                    px.width as usize,
                    px.height as usize,
                    &tab_infos,
                    bar_text(self.search.as_ref().map(|s| s.query.as_str()), focused_copy)
                        .as_deref(),
                    focused_claude,
                    elevate::title_badge(self.elevated),
                );
                if self.cockpit {
                    renderer.draw_cockpit(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        &cockpit_entries,
                        self.cockpit_sel,
                    );
                }
                if settings_open {
                    renderer.draw_settings(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        &render::SettingsView {
                            theme: settings_theme,
                            appearance: theme::appearance_highlight(&self.config),
                            font_family: &settings_family,
                            font_size: settings_size,
                            arabic_font: &settings_arabic,
                            cursor: cursor_style,
                            cursor_blink: self.config.cursor_blink_on(),
                            scrollback: self.config.scrollback_lines(),
                            copy_on_select: self.config.copy_on_select_on(),
                            ligatures: self.config.ligatures.unwrap_or(true),
                            bell: self.config.bell_mode(),
                            notifications: self.config.notifications_on(),
                            padding: self.config.padding_px(),
                            opacity_pct: (self.config.opacity_level() * 100.0).round() as i32,
                            shell: &shells::label_for(
                                &self.shell_profiles,
                                &self.config.shell_program(),
                            ),
                            hide_single_tab: self.config.hide_single_tab_on(),
                            confirm_close: self.config.confirm_close_on(),
                        },
                    );
                }
                if let Some(p) = &self.pending_paste {
                    let all = card_preview(p.kind, &p.text);
                    let preview = if p.kind == PendingKind::ApproveLua {
                        lua_card_window(&all, p.scroll).0
                    } else {
                        all
                    };
                    let heading = (p.kind == PendingKind::ApproveLua)
                        .then(|| tr("تشغيل سكربت لوا؟", "Run this Lua script?"));
                    renderer.draw_paste_guard(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        &render::PasteCard {
                            lines: p.lines,
                            chars: p.chars,
                            reason: &p.reason,
                            preview: &preview,
                            heading,
                        },
                    );
                }
                if let Some(block) = &self.blocked_link {
                    if block.is_app_error {
                        renderer.draw_app_error(
                            &mut verts,
                            px.width as i32,
                            px.height as i32,
                            &block.reason,
                            &block.shown,
                        );
                    } else {
                        renderer.draw_link_block(
                            &mut verts,
                            px.width as usize,
                            px.height as usize,
                            &block.reason,
                            &block.shown,
                            block.confirm.is_some(),
                        );
                    }
                }
                if let Some((rows, sel, capturing, flash)) = &shortcuts_view {
                    renderer.draw_shortcuts(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        rows,
                        *sel,
                        *capturing,
                        flash.as_deref(),
                    );
                }
                if let Some((q, rows, sel, menu)) = &palette_view {
                    if let Some(anchor) = menu {
                        renderer.draw_menu(
                            &mut verts,
                            px.width as usize,
                            px.height as usize,
                            rows,
                            *sel,
                            *anchor,
                            q,
                        );
                    } else {
                        renderer.draw_palette(
                            &mut verts,
                            px.width as usize,
                            px.height as usize,
                            q,
                            rows,
                            *sel,
                        );
                    }
                }
                if let Some((names, previews, sel)) = &posh_view {
                    renderer.draw_posh(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        names,
                        previews,
                        *sel,
                    );
                }
                if let Some(msg) = &close_msg {
                    renderer.draw_close_guard(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        msg,
                    );
                }
                renderer.use_ui_metrics(false);
            }
            // renderer still warming up on its thread: clear-only dark frame
            _ => {}
        }
        if let (Some(notice), Some(renderer)) = (&config_notice, self.renderer.as_mut()) {
            renderer.use_ui_metrics(true);
            renderer.draw_config_notice(
                &mut verts,
                px.width as i32,
                px.height as i32,
                &notice.text,
                notice.until.is_none(),
            );
            renderer.use_ui_metrics(false);
        }
        // the IME candidate window sits at the focused cursor. After the
        // IME context is rebuilt the same box has to be sent again.
        let spot = self.renderer.as_ref().and_then(|r| r.ime_spot);
        if let Some((x, y, w, h)) = keyroute::ime_area_due(self.ime_spot, spot, self.ime_area_reset)
        {
            window.set_ime_cursor_area(
                PhysicalPosition::new(x, y),
                winit::dpi::PhysicalSize::new(w.max(1) as u32, h.max(1) as u32),
            );
            self.ime_spot = Some((x, y, w, h));
            self.ime_area_reset = false;
        }
        let Some(gpu) = self.gpu.as_mut() else { return };
        // During a Windows resize, the live client size can advance ahead of
        // Resized events. Use the same size for geometry and the swapchain.
        let surface_resized = gpu.resize(px.width, px.height);
        if surface_resized {
            self.last_frame_size = None;
        }
        // new glyphs were rasterized this frame: sync the atlas texture
        let mut heal = false;
        if let Some(renderer) = self.renderer.as_mut() {
            if renderer.atlas.dirty {
                gpu.upload_atlas(&renderer.atlas.pages);
                renderer.atlas.dirty = false;
            }
            // an atlas reset mid-frame leaves earlier quads with stale uvs:
            // one more redraw re-emits everything from the fresh atlas
            heal = renderer.atlas.generation != self.atlas_generation;
            self.atlas_generation = renderer.atlas.generation;
        }
        // BAYAN_ATLAS_STRESS: shape a big spread of unique glyphs before the
        // first present, forcing a page grow — proves the array texture path
        // draws page-1 glyphs without corrupting page 0 (no input injection).
        if dev_hook("BAYAN_ATLAS_STRESS").is_some() {
            if let Some(r) = self.renderer.as_mut() {
                // persistent overlay (re-emitted every frame) so the second
                // atlas page's glyphs stay on screen for verification
                r.stress_atlas(&mut verts, px.width as usize, px.height as usize);
                if r.atlas.dirty {
                    gpu.upload_atlas(&r.atlas.pages);
                    r.atlas.dirty = false;
                }
            }
        }
        // differential redraw: an identical quad set + unchanged atlas means
        // the frame is pixel-for-pixel the last one — skip the submit. (An
        // atlas upload this frame always redraws; so does a heal.)
        let frame_bytes = bytemuck::cast_slice::<gpu::Vertex, u8>(&verts);
        let mut extended = Vec::new();
        let frame_key: &[u8] = if sprites.is_empty() {
            frame_bytes
        } else {
            extended.extend_from_slice(frame_bytes);
            for sprite in &sprites {
                extended.extend_from_slice(&sprite.cookie.to_le_bytes());
                extended.extend_from_slice(bytemuck::cast_slice(&sprite.verts));
            }
            &extended
        };
        let unchanged =
            self.last_frame_size == Some(px) && !heal && frame_key == self.last_frame.as_slice();
        if unchanged {
            return;
        }
        let bg = self.renderer.as_ref().map_or(render::BG, |r| r.bg);
        let quads: Vec<gpu::ImageQuad<'_>> = sprites
            .iter()
            .map(|sprite| gpu::ImageQuad {
                cookie: sprite.cookie,
                width: sprite.width,
                height: sprite.height,
                rgba: &sprite.rgba,
                verts: &sprite.verts,
            })
            .collect();
        if !gpu.render(&verts, &quads, bg) {
            // Retry once after reconfiguring, without spinning on occlusion.
            self.last_frame_size = None;
            if surface_resized || heal {
                window.request_redraw();
            }
            return;
        }
        crate::prof::frame_presented();
        self.last_frame.clear();
        self.last_frame.extend_from_slice(frame_key);
        self.last_frame_size = Some(px);
        if heal {
            window.request_redraw();
        }
        if !self.first_frame {
            self.first_frame = true;
            window.set_visible(true);
            // winit reapplies ITS OWN style flags on visibility changes,
            // wiping the layered bit — restore the configured alpha after
            self.apply_opacity();
            crate::prof::mark("first frame presented");
        }
    }

    /// One PTY read. A live wheel or a touch still inside [`scroll::WHEEL_MS`]
    /// is kept; only a stale fraction at the bottom is dropped.
    fn wakeup(&mut self, id: u64) {
        self.link_cache.invalidate_pane(id);
        // The row that was hovered may have moved. Drop the tooltip
        // and the click target together, or they can disagree.
        if self.link_hover.as_ref().is_some_and(|h| h.pane_id == id) {
            self.clear_link_hover();
        }
        if !self.first_output {
            self.first_output = true;
            crate::prof::mark("first pty output");
        }
        if let Some((ti, pi)) = self.find_pane(id) {
            self.tabs[ti].panes[pi].last_output = Some(Instant::now());
            let (alt, following) = {
                let t = self.tabs[ti].panes[pi].session.term.lock();
                (
                    t.mode().contains(TermMode::ALT_SCREEN),
                    t.grid().display_offset() == 0,
                )
            };
            let was_alt = self.alt_seen.insert(id, alt).unwrap_or(false);
            let now = Instant::now();
            scroll::apply_alt_screen(&mut self.smooth, id, was_alt, alt);
            scroll::apply_output(&mut self.smooth, id, following, now);
            let ris = {
                let mut meta = self.tabs[ti].panes[pi].session.meta.lock_or_recover();
                let ris = meta.saw_ris;
                meta.saw_ris = false;
                ris
            };
            if ris {
                self.mouse.note_ris(id);
            }
        }
        // the chunk may have enabled focus reporting; tell the
        // program whether the window is looking at it
        self.sync_focus(None);
        self.request_redraw();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // hidden until the GPU presents the first dark frame (~0.4s): a
        // fully-drawn window appearing beats a white flash while DX12 warms up.
        // BAYAN_SHOW_NOW shows it as soon as the accessibility adapter exists.
        // AccessKit panics if the adapter is created on an already-visible window.
        let show_now = dev_hook("BAYAN_SHOW_NOW").is_some();
        // The exe's embedded resource covers Explorer and the taskbar, but the
        // title bar and Alt-Tab read the WINDOW icon, which winit only sets if
        // we hand it pixels. Raw RGBA rather than a decoder: 16KB, no new crate,
        // and generated from assets/bayan.ico so the two can't drift apart.
        const ICON_RGBA: &[u8] = include_bytes!("../assets/icon_64.rgba");
        let icon = winit::window::Icon::from_rgba(ICON_RGBA.to_vec(), 64, 64).ok();
        use winit::platform::windows::WindowAttributesExtWindows;
        let mut attrs = Window::default_attributes()
            // M24: no OS title bar. The tab strip IS the title bar, so the
            // window carries one row of chrome instead of two. We draw the
            // caption buttons and handle drag/resize ourselves below.
            .with_decorations(false)
            .with_undecorated_shadow(true)
            .with_title(elevate::window_title(self.elevated))
            .with_window_icon(icon)
            .with_visible(false)
            .with_inner_size(LogicalSize::new(1100.0, 700.0));
        // yesterday's rectangle, if it is still on a connected monitor
        let monitors: Vec<(i32, i32, u32, u32)> = el
            .available_monitors()
            .map(|m| {
                let (p, s) = (m.position(), m.size());
                (p.x, p.y, s.width, s.height)
            })
            .collect();
        if let Some(g) = load_saved_state().and_then(|s| s.window) {
            if geometry_on_screen(g, &monitors) {
                attrs = attrs
                    .with_position(winit::dpi::PhysicalPosition::new(g.x, g.y))
                    .with_inner_size(winit::dpi::PhysicalSize::new(g.w, g.h))
                    .with_maximized(g.maximized);
                self.normal_geom = Some(WindowGeom {
                    maximized: false,
                    ..g
                });
            }
        }
        let window = match el.create_window(attrs) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.startup_error = Some(format!("Could not create the window: {error}"));
                el.exit();
                return;
            }
        };
        crate::prof::mark("window created");
        // Before the first show: a reader that connects later is answered
        // from the UI thread, and nothing is built until then.
        self.reader = Some(accesskit_winit::Adapter::with_event_loop_proxy(
            el,
            &window,
            self.proxy.clone(),
        ));
        if show_now {
            window.set_visible(true);
        }
        // winit turns the IME off on every new window: CJK input, the
        // emoji panel (Win+.) and dictation (Win+H) need it on
        window.set_ime_allowed(true);
        let px = window.inner_size();
        let gpu = match gpu::Gpu::new(window.clone(), px.width, px.height) {
            Ok(gpu) => gpu,
            Err(error) => {
                self.startup_error = Some(format!("Could not initialize graphics: {error}"));
                el.exit();
                return;
            }
        };
        crate::prof::mark("gpu ready");
        self.gpu = Some(gpu);
        self.window = Some(window.clone());
        self.report_config_load_error();
        self.apply_admin_notice();
        self.report_lua_errors();
        self.show_next_lua_card();
        self.apply_opacity(); // the configured window alpha, from frame one
        self.apply_window_frame();
        if let Some(h) = self.hwnd() {
            // the tray icon: toast anchor + summon target
            if !toast::init(h) {
                eprintln!("bayan: tray icon registration failed — no toasts");
            }
            if let Ok(exe) = std::env::current_exe() {
                if let Ok(path) = cli::exe_display(&exe) {
                    let app_id = jumplist::app_user_model_id(&path);
                    if let Err(error) = jumplist::set_window_app_id(h, &app_id) {
                        eprintln!("bayan: window AppUserModelID: {error}");
                    }
                }
            }
        }
        // first frame NOW: a dark window on screen beats a frozen launcher.
        // The shell and the font system warm up behind it, in parallel.
        self.redraw();
        self.start_sessions();
        // FontSystem::new scans every installed font — the documented
        // cosmic-text startup cost (pop-os/cosmic-text#247): keep it off
        // the UI thread and swap the renderer in when it's ready
        self.rebuild_renderer();
        // the quake hotkey (Ctrl+Alt+`): a null-hwnd registration posts
        // WM_HOTKEY to this thread's queue, where the msg hook catches it
        // SAFETY: a null hwnd is the documented way to have WM_HOTKEY posted
        // to this thread's queue; the rest are plain integers. A chord another
        // program owns makes it return 0, which is ignored.
        unsafe {
            use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
                RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, VK_OEM_3,
            };
            RegisterHotKey(
                std::ptr::null_mut(),
                QUAKE_ID as i32,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                VK_OEM_3 as u32,
            );
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::ShellProfiles(profiles) => {
                if let Some(palette) = self.palette.as_mut() {
                    palette.pressed = None;
                    palette
                        .extras
                        .retain(|(row, _)| !matches!(row, PaletteRow::Shell(_)));
                    palette.extras.extend(profiles.iter().map(|shell| {
                        (
                            PaletteRow::Shell(shell.command.clone()),
                            format!("{} — {}", tr("تبويب جديد", "New tab"), shell.label()),
                        )
                    }));
                }
                if let Err(error) =
                    jumplist::refresh(&profiles, elevate::may_write_bayan(self.elevated))
                {
                    eprintln!("bayan: jump list: {error}");
                }
                self.shell_profiles = profiles;
                self.shell_profiles_ready = true;
                self.bump_reader();
                self.start_sessions();
                self.request_redraw();
            }
            UserEvent::Wakeup(id) => {
                self.bump_reader();
                self.wakeup(id);
            }
            UserEvent::PtyWrite(id, text) => {
                if let Some((ti, pi)) = self.find_pane(id) {
                    self.tabs[ti].panes[pi].session.write(text.as_bytes());
                }
            }
            UserEvent::PtyWriteFailed(id, error) => {
                if let Some((ti, pi)) = self.find_pane(id) {
                    // one report per failure burst; re-arm only once shown
                    self.tabs[ti].panes[pi].session.write_error_shown();
                    self.report_error(tr("تعذر إرسال الإدخال", "Could not send input"), &error);
                }
            }
            UserEvent::ClipboardLoad(_id, _kind) => {
                // No PTY reply. The read flag defaults off, and even when
                // it is on this build does not read the clipboard or write
                // an empty OSC 52. Item 31 is the read.
            }
            UserEvent::ClipboardSet(id, text) => {
                // has_focus, not the startup value of self.focused: that flag
                // begins false and only tracks Focused events, which can lag
                // the real window state (and used to begin as true).
                let window_focused = self.window.as_ref().is_some_and(|w| w.has_focus());
                if term::allow_osc52_write(
                    self.config.osc52_copy_enabled(),
                    window_focused,
                    self.osc52_pane_is_focused(id),
                ) {
                    self.set_clipboard(text);
                }
            }
            UserEvent::Bell(id) => {
                // Terminal output does not run Lua. The helper's result is empty.
                let _ = lua_host::plugin_sends_from_pty(
                    &self.lua_plugins,
                    &lua_host::PtyFacts {
                        bell: true,
                        ..lua_host::PtyFacts::default()
                    },
                );
                // the cockpit signal: a pane you're NOT looking at wants you
                let mode = self.config.bell_mode();
                if mode == config::BellMode::Silent {
                    return;
                }
                if let Some((ti, _)) = self.find_pane(id) {
                    let background = ti != self.active;
                    if background {
                        self.tabs[ti].attention = true;
                        self.request_redraw();
                    }
                    // sound only when you could have missed it: another tab,
                    // or the window itself is unfocused
                    if mode == config::BellMode::Sound && (background || !self.focused) {
                        beep();
                    }
                    // the window is unfocused: a dot nobody can see is no
                    // alert. A toast, like a finished command gets (Claude
                    // Code with preferredNotifChannel=terminal_bell rings
                    // exactly this bell when it needs an approval).
                    if !self.focused && self.config.notifications_on() {
                        let body = self.tabs[ti].label();
                        self.toast_from_pty(tr("بيان — تنبيه", "Bayan — alert"), &body);
                        if let Some(w) = &self.window {
                            w.request_user_attention(Some(
                                winit::window::UserAttentionType::Informational,
                            ));
                        }
                    }
                }
            }
            UserEvent::Notify(id, title, body) => {
                // an explicit ask (OSC 9 / 777 / 99): the program decided
                // this deserves attention, so the only question is whether
                // the user is already looking — same rule as the bell
                if let Some((ti, _)) = self.find_pane(id) {
                    let background = ti != self.active;
                    if background {
                        self.tabs[ti].attention = true;
                    }
                    // NEW-01: any output can carry these (ssh, `cat`, an
                    // install script), so the toast is opt-in, and its
                    // title is always Bayan's own — the program's text is
                    // sanitized by the scanner and lives in the body only
                    if (background || !self.focused) && self.config.osc_notify_on() {
                        let (title, body) = toast::pty_toast(&self.tabs[ti].label(), &title, &body);
                        self.toast_from_pty(&title, &body);
                        if !self.focused {
                            if let Some(w) = &self.window {
                                w.request_user_attention(Some(
                                    winit::window::UserAttentionType::Informational,
                                ));
                            }
                        }
                    }
                    self.request_redraw();
                }
            }
            UserEvent::Progress(id, p) => {
                // only on a real change: a prompt clears progress, and
                // prompts arrive constantly — redrawing for each one would
                // churn the frame state M14's differential redraw relies on
                if let Some((ti, pi)) = self.find_pane(id) {
                    if self.tabs[ti].panes[pi].progress != p {
                        self.tabs[ti].panes[pi].progress = p;
                        self.request_redraw(); // the tab bar and cockpit draw it
                    }
                }
            }
            UserEvent::CommandFinished(id, ok) => {
                // a long command ended while nobody was looking: flash the
                // taskbar + the amber tab dot, and (M19) a native toast
                // when the whole WINDOW is unfocused — a background tab
                // while you're looking is the dot's job, not a popup's
                if let Some((ti, _)) = self.find_pane(id) {
                    if !self.focused || ti != self.active {
                        self.tabs[ti].attention = true;
                        if let Some(w) = &self.window {
                            w.request_user_attention(Some(
                                winit::window::UserAttentionType::Informational,
                            ));
                        }
                        if self.config.notifications_on() && !self.focused {
                            let title = ok.title();
                            let body = self.tabs[ti].label();
                            self.toast_from_pty(title, &body);
                        }
                        self.request_redraw();
                    }
                }
            }
            UserEvent::Cwd(id, cwd) => {
                let _ = lua_host::plugin_sends_from_pty(
                    &self.lua_plugins,
                    &lua_host::PtyFacts {
                        cwd: Some(cwd.clone()),
                        title: Some(cwd.clone()),
                        ..lua_host::PtyFacts::default()
                    },
                );
                let base = cwd.rsplit(['\\', '/']).next().unwrap_or(&cwd).to_string();
                if let Some((ti, pi)) = self.find_pane(id) {
                    // the tab is named after its FOCUSED pane's directory
                    if self.tabs[ti].focused == pi {
                        self.tabs[ti].title = base;
                    }
                }
                self.request_redraw(); // tab titles live in the frame
            }
            UserEvent::RendererReady(r) => {
                crate::prof::mark("renderer ready");
                // The build cloned the config when it started. A theme
                // signal during that build has a newer sample; paint it.
                let mut r = *r;
                let colors = render::resolved_colors(&self.config);
                r.set_colors(colors.bg, colors.fg, colors.palette);
                term::set_query_colors(r.query_colors());
                self.renderer = Some(r);
                self.renderer_building = false;
                self.apply_window_frame();
                // now that cell metrics exist, snap every pane to its rect
                self.relayout_active();
                // A reload, a zoom, or an accessibility change that arrived
                // while this build ran is not in the renderer we just
                // installed. Start the next one from what the session wants
                // now.
                if self.config_build.finish(self.font_delta) || self.a11y != self.inflight_a11y {
                    self.rebuild_renderer();
                }
                if let Some(text) = self.debug_paste.take() {
                    self.debug_right_click_paste(text);
                }
                self.request_redraw();
            }
            UserEvent::Exit(id) => {
                // that pane's shell ended: close it; the last pane of the
                // last tab closes Bayan
                if let Some((ti, pi)) = self.find_pane(id) {
                    self.remove_pane(ti, pi, el);
                }
            }
            UserEvent::PoshPreview(generation, i, preview) => {
                if let Some(p) = self.posh.as_mut() {
                    if p.generation == generation && i < p.previews.len() {
                        p.previews[i] = preview;
                        self.request_redraw();
                    }
                }
            }
            UserEvent::PoshDropped(generation, dropped) => {
                let again = self.posh.as_mut().is_some_and(|p| {
                    if p.generation != generation {
                        return false;
                    }
                    let sel = p.sel;
                    let arrived: Vec<bool> = p.previews.iter().map(Option::is_some).collect();
                    release_dropped_posh(&mut p.asked, &arrived, &dropped, sel)
                });
                if again {
                    // the arrow is already on a row this burst did not run
                    self.request_posh_preview();
                }
            }
            UserEvent::LinkFailed { reason, shown } => {
                self.blocked_link = Some(BlockedLink {
                    reason,
                    shown: link::preview(&shown, 160),
                    confirm: None,
                    is_app_error: false,
                });
                self.clear_link_hover();
                self.request_redraw();
            }
            UserEvent::LinkConfirm { path } => {
                self.blocked_link = Some(BlockedLink {
                    reason: tr(
                        "لا يوجد محرّر في Program Files",
                        "No editor under Program Files",
                    )
                    .to_string(),
                    shown: path.clone(),
                    confirm: Some(link::ConfirmOpen::RevealOnly(path)),
                    is_app_error: false,
                });
                self.clear_link_hover();
                self.request_redraw();
            }
            UserEvent::ConfigReloaded(msg) => self.apply_config_reload(msg),
            UserEvent::AccessKit(event) => match event.window_event {
                accesskit_winit::WindowEvent::InitialTreeRequested => {
                    // The next consider must build even if the stamp matches
                    // a tree we never managed to send.
                    self.reader_seen_client = true;
                    self.reader_pace = screen_reader::TreePace::default();
                    self.publish_reader();
                }
                accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                    // Windows does not deliver this. Other backends do,
                    // and then the redraw path goes quiet again.
                    self.reader_seen_client = false;
                    self.reader_pace = screen_reader::TreePace::default();
                    self.reader_tree_at = None;
                    self.reader_gate.reset();
                    self.reader_bodies.clear();
                    self.reader_pane = None;
                    self.reader_mark = None;
                    self.reader_mark_ready = false;
                    self.reader_deadline = None;
                    self.reader_live = None;
                    self.reader_cmd.clear();
                }
                accesskit_winit::WindowEvent::ActionRequested(_) => {}
            },
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // quake toggle: summon Bayan from anywhere / tuck it away again
        if QUAKE_HIT.swap(false, std::sync::atomic::Ordering::Relaxed) {
            if let Some(w) = &self.window {
                if self.focused && w.is_visible().unwrap_or(true) {
                    w.set_visible(false);
                } else {
                    w.set_visible(true);
                    w.set_minimized(false);
                    w.focus_window();
                    self.apply_opacity(); // set_visible wipes the layered bit
                }
            }
        }
        // a toast/tray click summons (never hides — that's the quake key)
        if SUMMON_HIT.swap(false, std::sync::atomic::Ordering::Relaxed) {
            if let Some(w) = &self.window {
                w.set_visible(true);
                w.set_minimized(false);
                w.focus_window();
                self.apply_opacity(); // set_visible wipes the layered bit
            }
        }
        // High Contrast or the text-size slider changed. Re-read now; a
        // rebuild applies it on the next frame, without restarting.
        if A11Y_DIRTY.swap(false, std::sync::atomic::Ordering::Relaxed) {
            self.reload_a11y();
        }
        // busy dots decay after ~2s of quiet: keep repainting on a slow
        // heartbeat only while any background tab is (or just was) active
        let any_busy = self
            .tabs
            .iter()
            .enumerate()
            .any(|(i, t)| t.busy(i == self.active));
        let mut wake: Option<Instant> = None;
        if any_busy {
            wake = Some(Instant::now() + std::time::Duration::from_millis(600));
            self.request_redraw();
        }
        // cursor blink: wake exactly at the next phase flip (or the 15s
        // park); an idle window schedules nothing and truly sleeps
        let (blink_on, blink_wake) = self.cursor_blink_state();
        if blink_on != self.blink_drawn_on {
            self.request_redraw();
        }
        if let Some(b) = blink_wake {
            wake = Some(wake.map_or(b, |w| w.min(b)));
        }
        if let Some(t) = self.tick_smooth_scroll() {
            wake = Some(wake.map_or(t, |w| w.min(t)));
        }
        // a scrollbar that auto-hides wakes once, then the next frame drops
        // it and the window goes back to sleeping
        let tick = self
            .bars
            .tick(self.config.scrollbar.autohide_on(), Instant::now());
        if tick.redraw {
            self.request_redraw();
        }
        if let Some(t) = tick.wake {
            wake = Some(wake.map_or(t, |w| w.min(t)));
        }
        if let Some(until) = self.config_notice.as_ref().and_then(|notice| notice.until) {
            if Instant::now() >= until {
                self.config_notice = None;
                self.request_redraw();
            } else {
                wake = Some(wake.map_or(until, |w| w.min(until)));
            }
        }
        // The burst is spoken once the hold elapses, and a stamp that
        // arrived inside the 10 Hz window is published when that window ends.
        let now = Instant::now();
        if self.reader_deadline.is_some_and(|t| now >= t)
            || self.reader_tree_at.is_some_and(|t| now >= t)
        {
            self.publish_reader();
        }
        let soon = match (self.reader_deadline, self.reader_tree_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(at) = soon {
            wake = Some(wake.map_or(at, |w| w.min(at)));
        }
        match wake {
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let (Some(adapter), Some(window)) = (self.reader.as_mut(), self.window.as_ref()) {
            adapter.process_event(window, &event);
        }
        if self.modal_pointer(&event, el) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => self.request_close(el),
            WindowEvent::Focused(f) => self.focus_changed(f),
            WindowEvent::DroppedFile(path) => self.drop_file(&path),
            WindowEvent::ModifiersChanged(m) => self.modifiers_changed(m.state()),
            WindowEvent::ScaleFactorChanged { .. } => {
                // HiDPI: the window moved to a monitor with another scale.
                // Rebuild cell metrics off-thread; the current renderer keeps
                // painting until RendererReady swaps it in and re-snaps the grid.
                self.rebuild_renderer();
            }
            WindowEvent::Moved(_) => self.track_geometry(),
            WindowEvent::Resized(px) => self.resized(px),
            WindowEvent::CursorMoved { position, .. } => self.cursor_moved(position),
            WindowEvent::CursorLeft { .. } => {
                self.clear_link_hover();
                self.clear_scrollbar_pointer();
                if let Some(r) = self.renderer.as_mut() {
                    if r.hover_tab.take().is_some() {
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => self.mouse_input(state, button, el),
            WindowEvent::MouseWheel { delta, .. } => self.mouse_wheel(delta),
            WindowEvent::KeyboardInput { event, .. } => self.key_input(&event, el),
            WindowEvent::Ime(ime) => self.ime_input(ime),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::ThemeChanged(theme) => {
                // The registry DWORD is the app theme. winit's value covers a
                // read that fails (the key is missing, or this is not Windows).
                let event = match theme::read_apps_use_light_theme() {
                    Some(value) => theme::SystemThemeEvent::ImmersiveColorSet {
                        apps_use_light_theme: Some(value),
                    },
                    None => theme::SystemThemeEvent::Winit {
                        light: matches!(theme, winit::window::Theme::Light),
                    },
                };
                self.on_system_theme(event);
            }
            _ => {}
        }
    }
}

/// Focus loss forgets presses that never reached the PTY. The release
/// often goes to whichever window is focused now, so the set would
/// otherwise keep swallowing that physical key forever.
fn blur_forgets_held_keys(focused: bool, held: &mut Vec<winit::keyboard::PhysicalKey>) {
    if !focused {
        held.clear();
    }
}

/// Drop the current directory from the DLL search path before anything else
/// in this process loads a library by name.
///
/// `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_APPLICATION_DIR)`
/// (0x800 | 0x200) is available on Windows 8+ and is intentionally not
/// called. wgpu-hal loads `d3d12.dll` and `dxgi.dll` by bare name
/// (`libloading` → `LoadLibraryW`). The inbox `d3d12.dll` then maps
/// `D3D12Core.dll` from `%SystemRoot%\System32\D3D12`, and the GL fallback
/// loads an ICD from `DriverStore\FileRepository\...`. Those neighbours are
/// not System32 itself and not the application directory.
/// `SetDefaultDllDirectories` replaces the process search path permanently
/// and does not accept `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR` (that flag is
/// per `LoadLibraryEx`). A missed dependency there is a black window, and
/// neither CI nor this call can create a device to prove it still works.
/// `SetDllDirectoryW("")` only removes the CWD, which is what Microsoft
/// documents for this mitigation, and leaves System32, the application
/// directory, and PATH in place for DirectX and GL.
///
/// ConPTY does not depend on this call. `CreatePseudoConsole`,
/// `ResizePseudoConsole`, and `ClosePseudoConsole` come from `kernel32.dll`
/// opened with `LoadLibraryExW(L"kernel32.dll", NULL, LOAD_LIBRARY_SEARCH_SYSTEM32)`.
/// `conpty.dll` is never passed to the loader.
///
/// NEW-04 (partly): the application directory still precedes System32 for a
/// bare name, so a `d3d12.dll` planted beside `bayan.exe` (a Downloads
/// folder, a writable `target\release`) would load. Without touching the
/// search order above, the graphics DLLs wgpu loads DYNAMICALLY by bare name
/// are mapped first from System32 only (`LOAD_LIBRARY_SEARCH_SYSTEM32`); a
/// later `LoadLibraryW("d3d12.dll")` returns the module already in memory.
/// Their own dependencies (D3D12Core, the DriverStore ICDs) resolve as before.
///
/// NOT covered: `dxgi.dll` and `opengl32.dll` are STATIC imports of the exe
/// (wgpu-hal's `windows` raw-dylib calls), resolved by the loader before
/// `main` runs, so nothing here can reach them. Closing that needs
/// `/DEPENDENTLOADFLAG:0x800` at link time, which also restricts every other
/// static import (the VC runtime among them) to System32 — untested, so not
/// done. Until then: install Bayan in a folder only you can write
/// (`%LOCALAPPDATA%\Programs\Bayan`), not run it from Downloads.
pub(crate) fn harden_dll_search() {
    use windows_sys::Win32::System::LibraryLoader::{
        LoadLibraryExW, SetDllDirectoryW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    // An empty string removes the CWD. NULL would put it back.
    let empty = [0u16];
    // SAFETY: `empty` is a NUL-terminated UTF-16 string that outlives the
    // call, and SetDllDirectoryW copies it rather than keeping the pointer.
    unsafe {
        let _ = SetDllDirectoryW(empty.as_ptr());
    }
    for name in GPU_SYSTEM_DLLS {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the file
        // handle argument must be null, and the flag restricts the search to
        // System32. The module stays loaded for the life of the process on
        // purpose, so the returned handle is not freed.
        unsafe {
            let _ = LoadLibraryExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
        }
    }
}

/// Graphics DLLs wgpu-hal loads dynamically by bare name on Windows (DX12
/// path; `gpu.rs` asks for DX12 | GL), pre-mapped from System32 by
/// `harden_dll_search` (NEW-04). Not listed: `dxgi.dll` / `opengl32.dll`
/// (static imports, already loaded before `main`), and `dxcompiler.dll` /
/// `libEGL.dll`, which are not inbox System32 components (pinning an old
/// inbox copy could break the DXC path).
const GPU_SYSTEM_DLLS: &[&str] = &["d3d12.dll", "dcomp.dll", "d3dcompiler_47.dll"];

fn show_error_dialog(
    owner: Option<windows_sys::Win32::Foundation::HWND>,
    title: &str,
    detail: &str,
) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    let detail: Vec<u16> = detail.encode_utf16().chain(Some(0)).collect();
    // SAFETY: both buffers are NUL-terminated and live for the call.
    unsafe {
        MessageBoxW(
            owner.unwrap_or(std::ptr::null_mut()),
            detail.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn show_info_dialog(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

    let title: Vec<u16> = "Bayan".encode_utf16().chain(Some(0)).collect();
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    // SAFETY: both buffers are NUL-terminated and live for the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// Help, version, and menu registration have no terminal yet. A console
/// parent (cmd) sees the text; Explorer gets a dialog instead of silence.
fn present(text: &str) {
    if !cli::write_console(text) {
        show_info_dialog(text);
    }
}

/// What a normal launch carries into the window. The folder is already gated.
struct Startup {
    directory: Option<String>,
    profile_command: Option<String>,
    command: Option<Vec<String>>,
    /// `Some` when `-p` already ran shell detection, so the window does not
    /// detect a second time.
    shells: Option<Vec<shells::Shell>>,
    elevated: bool,
}

fn main() -> std::process::ExitCode {
    match dispatch() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bayan: {error}");
            show_error_dialog(None, tr("تعذّر بدء بيان", "Bayan could not run"), &error);
            std::process::ExitCode::FAILURE
        }
    }
}

fn dispatch() -> Result<(), String> {
    // The mux server is this executable started with exactly this one
    // argument. It is not a user-facing flag, so `cli::parse` does not know
    // it, and it never opens a window.
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|a| a == "--mux-server") && args.next().is_none() {
        return mux::run_mux_server();
    }
    match cli::parse(std::env::args_os().skip(1))? {
        cli::CliAction::Help => {
            present(cli::HELP);
            Ok(())
        }
        cli::CliAction::Version => {
            present(&format!("bayan {}", env!("CARGO_PKG_VERSION")));
            Ok(())
        }
        cli::CliAction::RegisterContextMenu => {
            context_menu::set_enabled(true)?;
            present(context_menu::REGISTERED_NOTE);
            Ok(())
        }
        cli::CliAction::UnregisterContextMenu => {
            context_menu::set_enabled(false)?;
            present(context_menu::REMOVED_NOTE);
            Ok(())
        }
        cli::CliAction::RegisterTerminalHandoff => {
            handoff::set_enabled(true)?;
            present(handoff::REGISTERED_NOTE);
            Ok(())
        }
        cli::CliAction::UnregisterTerminalHandoff => {
            handoff::set_enabled(false)?;
            present(handoff::REMOVED_NOTE);
            Ok(())
        }
        cli::CliAction::Handoff => handoff::serve(),
        cli::CliAction::Launch(launch) => {
            let requested = launch.elevated;
            // The badge follows the token, including a window Explorer
            // started with "Run as administrator" and no `--elevated`.
            // A failed token query counts as elevated, so the restrictions
            // stay on. Only the flag relaunches, and only when the query
            // succeeds and says this process is not elevated.
            let elevated = requested || elevate::process_is_elevated();
            // `--elevated` relaunches through UAC unless this process is
            // already the elevated one. The child receives only the
            // allowlisted argv, then resolves `-p` from its own detection.
            let launch = if requested {
                let forwarded = elevate::forward_args(&launch)?;
                // A name that is not installed fails before the UAC prompt.
                // The command that goes with it is not kept: the elevated
                // process detects its shells again and matches the name.
                if let Some(name) = launch.profile.as_deref() {
                    let found = shells::detect();
                    shells::require_profile_by_name(&found, name)?;
                }
                if !elevate::process_is_elevated() {
                    let exe = std::env::current_exe()
                        .map_err(|error| format!("could not find bayan.exe: {error}"))?;
                    elevate::relaunch(&exe, &forwarded)?;
                    return Ok(());
                }
                match cli::parse(forwarded)? {
                    cli::CliAction::Launch(clean) => clean,
                    _ => {
                        return Err("elevation relaunch was not a launch".into());
                    }
                }
            } else {
                launch
            };
            // Gate the folder before shell detection: a refused path should
            // not wait on `wsl.exe`. An elevated launch has no folder.
            let directory = match launch.directory {
                Some(raw) if !elevated => Some(cli::gate_start_dir(&raw)?),
                Some(_) | None => None,
            };
            let (profile_command, shells) = if elevated {
                let found = shells::detect();
                let command = match launch.profile.as_deref() {
                    Some(name) => shells::require_profile_by_name(&found, name)?
                        .command
                        .clone(),
                    None => shells::elevated_default_command(&found),
                };
                (Some(command), Some(found))
            } else {
                match launch.profile {
                    Some(name) => {
                        let found = shells::detect();
                        let shell = shells::require_profile(&found, &name)?;
                        (Some(shell.command.clone()), Some(found))
                    }
                    None => (None, None),
                }
            };
            run(Startup {
                directory,
                profile_command,
                command: if elevated { None } else { launch.command },
                shells,
                elevated,
            })
        }
    }
}

/// The directory half of [`cli::parse`], gated the way `#45` gated it.
/// The test below calls this so those cases stay on the same function.
#[cfg(test)]
fn cli_start_dir(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Option<String>, String> {
    cli::start_directory(args)
}

fn run(startup: Startup) -> Result<(), String> {
    harden_dll_search();
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(path) = cli::exe_display(&exe) {
            let app_id = jumplist::app_user_model_id(&path);
            if let Err(error) = jumplist::set_process_app_id(&app_id) {
                eprintln!("bayan: AppUserModelID: {error}");
            }
        }
    }
    prof::init();
    prof::mark("main start");
    use winit::platform::windows::EventLoopBuilderExtWindows;
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .with_msg_hook(|msg| {
            use windows_sys::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY};
            // SAFETY: winit's msg hook is handed a pointer to the MSG being
            // dispatched, valid for the duration of the callback. It is only read.
            let m = unsafe { &*(msg as *const MSG) };
            if a11y::message_triggers_reread(m.message) {
                A11Y_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if m.message == WM_HOTKEY && m.wParam == QUAKE_ID {
                QUAKE_HIT.store(true, std::sync::atomic::Ordering::Relaxed);
                return true; // consumed
            }
            // tray icon / toast callback: a click summons the window
            if m.message == toast::TRAY_MSG {
                let event = (m.lParam as u32) & 0xffff;
                if event == toast::NIN_BALLOONUSERCLICK || event == toast::WM_LBUTTONUP {
                    SUMMON_HIT.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return true; // ours either way
            }
            false
        })
        .build()
        .map_err(|error| format!("Could not create the event loop: {error}"))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    prof::mark("event loop built");
    let proxy = event_loop.create_proxy();
    let shells_ready = startup.shells.is_some();
    if !shells_ready {
        let profiles_proxy = proxy.clone();
        std::thread::spawn(move || {
            let _ = profiles_proxy.send_event(UserEvent::ShellProfiles(shells::detect()));
        });
    }
    let (mut cfg, config_load_error) = match config::load() {
        Ok(cfg) => (cfg, None),
        Err(error) => (config::UserConfig::default(), Some(error.to_string())),
    };
    cfg.system_apps_light =
        theme::read_apps_use_light_theme().map(|v| theme::apps_use_light_theme(Some(v)));
    // config.lua merges over JSON only when its bytes are already approved
    // and the token is not elevated. An elevated token (UAC or Explorer's
    // Run as administrator) keeps the JSON and runs no script and no plugin.
    let lua = lua_host::load_with(&cfg, &config::bayan_dir(), startup.elevated);
    for err in &lua.errors {
        eprintln!("bayan: {err}");
    }
    if let Some(notice) = &lua.notice {
        eprintln!("bayan: {notice}");
    }
    let lua_pending = lua.pending;
    let lua_plugins = lua.plugins;
    let lua_errors = lua.shown;
    let lua_notice = lua.notice;
    let json_config = lua.persisted;
    cfg = lua.config;
    // debug hook: BAYAN_ARABIC_FONT=<family> draws terminal Arabic in that
    // font for this run only (config.json is not written unless a setting
    // is changed in the panel), so the setting is screenshot-verifiable
    if let Some(v) = dev_hook("BAYAN_ARABIC_FONT") {
        cfg.arabic_font = v.to_str().map(str::to_string);
    }
    // plugins load once, before the first renderer, so a plugin theme named in
    // the config resolves on the very first frame. An elevated token loads
    // none: ~/.bayan/plugins is writable before UAC, and a send would reach
    // the elevated shell after the confirm card.
    let plug = plugins::init(startup.elevated, &cfg.disabled_plugins());
    if !plug.failed.is_empty() {
        eprintln!(
            "bayan: {} plugin(s) failed — see {}",
            plug.failed.len(),
            config::bayan_dir().join("plugins.log").display()
        );
    }
    // Optional installer / settings hook: the key is absent unless someone
    // set it. Failure here must not stop the terminal from opening.
    if let Some(enabled) = cfg.explorer_context_menu {
        // Skip the write when the verbs already match this exe, or are already gone.
        if let Err(error) = context_menu::sync_enabled(enabled) {
            eprintln!("bayan: Explorer menu: {error}");
        }
    }
    if let Some(enabled) = cfg.terminal_handoff {
        if let Err(error) = handoff::set_enabled(enabled) {
            eprintln!("bayan: terminal handoff: {error}");
        }
    }
    lang::set(cfg.lang());
    if let Some(shells) = &startup.shells {
        if let Err(error) = jumplist::refresh(shells, elevate::may_write_bayan(startup.elevated)) {
            eprintln!("bayan: jump list: {error}");
        }
    }
    // debug hook: BAYAN_LANG=en starts in English without touching
    // config.json, so the English layout can be screenshot-verified
    if let Some(v) = dev_hook("BAYAN_LANG") {
        lang::set(lang::Lang::from_config(v.to_str()));
    }
    term::set_osc52_read(cfg.osc52_read_enabled());
    let config_path = config::config_path();
    let baseline_bytes = std::fs::read(&config_path).ok();
    let reload_proxy = proxy.clone();
    let config_watch = config_watch::spawn(
        config_path,
        // the file's own content: config.lua's overrides are not in it
        json_config.clone(),
        baseline_bytes.as_deref(),
        move |msg| {
            let _ = reload_proxy.send_event(UserEvent::ConfigReloaded(msg));
        },
    );
    let mut app = App::new(proxy, cfg, config_load_error, startup.directory);
    app.config_watch = config_watch;
    app.shell_profiles = startup.shells.unwrap_or_default();
    app.shell_profiles_ready = shells_ready;
    app.start_profile_command = startup.profile_command;
    app.start_command = startup.command;
    app.elevated = startup.elevated;
    // App::new starts neutral so a test never reads the machine's High
    // Contrast theme; the running window reads it here.
    app.a11y = a11y::read_snapshot();
    app.inflight_a11y = app.a11y;
    app.json_config = json_config;
    app.lua_plugins = lua_plugins;
    app.lua_pending = lua_pending;
    app.lua_errors = lua_errors;
    app.lua_notice = lua_notice;
    let result = event_loop.run_app(&mut app);
    if let Some(error) = app.startup_error {
        return Err(error);
    }
    result.map_err(|error| format!("The event loop failed: {error}"))
}

/// The one winit event loop this process may own, as a proxy.
///
/// winit 0.30 allows a single `EventLoop` per process. A second `build`
/// fails with `EventLoopError::RecreationAttempt`, and dropping the loop does not clear
/// that flag. Every test that needs a proxy must call this and must not
/// build its own loop — a second builder in another test module panics
/// whichever of the two runs second. The loop is leaked so the proxy's
/// window target stays alive for the rest of the process.
#[cfg(test)]
pub(crate) fn test_proxy() -> EventLoopProxy<UserEvent> {
    use std::sync::OnceLock;
    use winit::platform::windows::EventLoopBuilderExtWindows;
    static PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();
    PROXY
        .get_or_init(|| {
            let el = EventLoop::<UserEvent>::with_user_event()
                .with_any_thread(true)
                .build()
                .expect("event loop");
            let proxy = el.create_proxy();
            std::mem::forget(el);
            proxy
        })
        .clone()
}

/// OSC 52 writes only from the focused pane of the active tab.
/// `tabs` is `(focused index, pane ids)` per tab. A background tab's pane
/// is not the focused slot of `active`, so it does not match.
fn osc52_pane_is_focused(tabs: &[(usize, &[u64])], active: usize, source: u64) -> bool {
    let Some((focused, panes)) = tabs.get(active) else {
        return false;
    };
    panes.get(*focused) == Some(&source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bayan-save-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn panel_cursor_survives_an_external_padding_edit() {
        // The last loaded file has the default cursor. The open panel set
        // the cursor to bar and has not saved. An external edit changes
        // only padding. The cursor stays bar; padding comes from the file.
        let base = config::UserConfig::default();
        let disk = config::encode(&base).unwrap();
        let from_editor = config::UserConfig {
            padding: Some(16),
            ..config::UserConfig::default()
        };
        let msg = config_watch::reload_from_bytes(
            base.clone(),
            &disk,
            &config::encode(&from_editor).unwrap(),
        )
        .expect("padding edit reloads");
        let config_watch::ReloadMsg::Applied {
            base: loaded_base, ..
        } = &msg
        else {
            panic!("padding edit parsed");
        };
        assert_eq!(
            loaded_base.padding, None,
            "the merge base is the previous file"
        );
        let live = config::UserConfig {
            cursor_style: Some("bar".into()),
            copy_on_select: Some(false),
            ..config::UserConfig::default()
        };
        let decision = decide_reload(&live, true, msg);
        assert_eq!(decision.config.cursor(), config::CursorStyle::Bar);
        assert!(!decision.config.copy_on_select_on());
        assert_eq!(decision.config.padding_px(), 16);
        assert!(decision.plan.padding);
        assert!(!decision.plan.cursor);
        assert!(
            decision.replaced.is_none(),
            "the file did not touch the cursor, so it does not announce an override"
        );
        // The next reload's base is the file we just loaded, not the merge.
        // A later file that still leaves the cursor unset keeps bar.
        let again = config::UserConfig {
            padding: Some(8),
            ..from_editor.clone()
        };
        let second = config_watch::ReloadMsg::Applied {
            config: Box::new(again),
            base: Box::new(from_editor),
            plan: config_watch::ApplyPlan::default(),
            deferred: Vec::new(),
        };
        let decision = decide_reload(&decision.config, true, second);
        assert_eq!(decision.config.cursor(), config::CursorStyle::Bar);
        assert_eq!(decision.config.padding_px(), 8);
    }

    /// The panel and the file both changed the font. The file wins that
    /// field. The panel's other unsaved edits stay.
    #[test]
    fn file_wins_when_the_panel_and_the_file_change_the_same_field() {
        let base = config::UserConfig {
            font_size: Some(14.0),
            ..config::UserConfig::default()
        };
        let live = config::UserConfig {
            font_size: Some(20.0),
            cursor_style: Some("bar".into()),
            ..base.clone()
        };
        let file = config::UserConfig {
            font_size: Some(18.0),
            ..base.clone()
        };
        let msg = config_watch::ReloadMsg::Applied {
            config: Box::new(file),
            base: Box::new(base),
            plan: config_watch::ApplyPlan::default(),
            deferred: Vec::new(),
        };
        let decision = decide_reload(&live, true, msg);
        assert_eq!(
            decision.config.font_size,
            Some(18.0),
            "the file's font wins the field both sides changed"
        );
        assert_eq!(decision.config.cursor(), config::CursorStyle::Bar);
        assert!(decision.plan.font, "20 on screen versus 18 from the file");
        assert!(decision.replaced.is_some());
    }

    /// `apply_config_reload` takes its plan from [`decide_reload`], which
    /// diffs the live config against the merge. Comparing the raw file to
    /// itself would drop the padding rebuild; comparing the live config to
    /// the raw file would rebuild a font the merge kept.
    #[test]
    fn reload_plan_compares_the_live_config_with_the_merge() {
        let base = config::UserConfig {
            font_size: Some(14.0),
            ..config::UserConfig::default()
        };
        let disk = config::encode(&base).unwrap();
        let from_editor = config::UserConfig {
            font_size: Some(14.0),
            padding: Some(12),
            ..config::UserConfig::default()
        };
        let msg =
            config_watch::reload_from_bytes(base, &disk, &config::encode(&from_editor).unwrap())
                .expect("padding edit reloads");
        let config_watch::ReloadMsg::Applied {
            plan: disk_plan,
            config: file,
            ..
        } = &msg
        else {
            panic!("the external edit parsed");
        };
        assert!(!disk_plan.font);
        assert!(disk_plan.padding);
        let live = config::UserConfig {
            font_size: Some(20.0),
            ..config::UserConfig::default()
        };
        // The comparison this test guards. Dropping `live` (diffing the
        // file with itself) makes padding false. Diffing `live` with the
        // raw file makes font true.
        let against_raw = plan_against_live(&live, file);
        assert!(against_raw.0.font);
        let decision = decide_reload(&live, false, msg);
        assert_eq!(decision.config.font_size, Some(20.0));
        assert_eq!(decision.config.padding_px(), 12);
        assert!(
            !decision.plan.font,
            "the merge kept 20, which is already on screen"
        );
        assert!(decision.plan.padding);
        assert!(decision.replaced.is_none());
    }

    #[test]
    fn truncated_config_reload_blocks_saving() {
        let current = config::UserConfig {
            font_size: Some(22.0),
            cursor_style: Some("bar".into()),
            osc52: Some("copy".into()),
            ..config::UserConfig::default()
        };
        let bytes = config::encode(&current).unwrap();
        let msg = config_watch::reload_from_bytes(
            current.clone(),
            &bytes,
            br#"{"font_size":99.0,"cursor_style":"underline""#,
        )
        .expect("a truncated file is a reload message");
        let decision = decide_reload(&current, false, msg);
        assert!(
            decision.load_error.is_some(),
            "truncated json is a load error"
        );
        assert_eq!(decision.config.font_size, Some(22.0));
        assert_eq!(decision.config.cursor(), config::CursorStyle::Bar);
        assert!(decision.config.osc52_copy_enabled());
        assert!(save_bytes(&decision.load_error, &decision.config).is_err());
        let dir = temp_dir("truncated");
        let path = dir.join("config.json");
        assert!(!path.exists(), "a blocked save must not create config.json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_json_reload_blocks_saving() {
        let current = config::UserConfig {
            font_size: Some(22.0),
            arabic_font: Some("Amiri".into()),
            ..config::UserConfig::default()
        };
        let bytes = config::encode(&current).unwrap();
        let msg = config_watch::reload_from_bytes(current.clone(), &bytes, b"{oops")
            .expect("invalid json is a reload message");
        let decision = decide_reload(&current, false, msg);
        assert!(decision.load_error.is_some());
        assert_eq!(decision.config.arabic_font.as_deref(), Some("Amiri"));
        assert_eq!(decision.config.font_size, Some(22.0));
        assert!(save_bytes(&decision.load_error, &decision.config).is_err());
    }

    #[test]
    fn deleting_config_json_clears_the_load_error_and_save_recreates_the_file() {
        let current = config::UserConfig {
            font_size: Some(22.0),
            cursor_style: Some("bar".into()),
            ..config::UserConfig::default()
        };
        // An earlier bad file is blocking saves. Deleting the file is not
        // another bad file: the running config stays, and a save writes it
        // back, never defaults over the user's settings.
        let [bad, deleted] = config_watch::reload_bad_then_missing(current.clone(), b"{oops");
        let mut blocked = decide_reload(&current, false, bad.unwrap());
        assert!(blocked.load_error.is_some());
        blocked = decide_reload(&blocked.config, false, deleted.expect("delete is noticed"));
        assert!(
            blocked.load_error.is_none(),
            "a missing file must not block save_config"
        );
        assert_eq!(blocked.config, current);
        let bytes = save_bytes(&blocked.load_error, &blocked.config).unwrap();
        let dir = temp_dir("deleted");
        let path = dir.join("config.json");
        config::write_bytes_at(&path, &bytes).unwrap();
        let restored = config::parse_bytes(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored, current);
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Enough of an `App` to run `on_system_theme`. No window, so the caption
    /// flag is computed and then has nowhere to go; the renderer and the
    /// config sample are what the test can see.
    fn app_for_theme(cfg: config::UserConfig) -> App {
        let renderer = render::Renderer::new(1.0, &cfg, 0.0);
        let mut app = App::new(test_proxy(), cfg, None, None);
        app.startup_pending = false;
        app.renderer = Some(renderer);
        app.size = TermSize { cols: 80, rows: 24 };
        app
    }

    /// The window loop's theme handler. Removing `on_system_theme` fails
    /// this test: the renderer must take the new palette, and the config
    /// must record the sample. A named scheme stays put.
    #[test]
    fn on_system_theme_paints_the_renderer_and_records_the_sample() {
        let cfg = config::UserConfig {
            system_apps_light: Some(false),
            ..Default::default()
        };
        let mut app = app_for_theme(cfg);
        assert_eq!(app.renderer.as_ref().map(|r| r.bg), Some(theme::DARK.bg));
        app.on_system_theme(theme::SystemThemeEvent::Winit { light: true });
        assert_eq!(app.config.system_apps_light, Some(true));
        assert_eq!(
            app.renderer.as_ref().map(|r| r.bg),
            Some(theme::LIGHT.bg),
            "a light ThemeChanged must reach the renderer"
        );
        app.on_system_theme(theme::SystemThemeEvent::Winit { light: false });
        assert_eq!(app.config.system_apps_light, Some(false));
        assert_eq!(app.renderer.as_ref().map(|r| r.bg), Some(theme::DARK.bg));

        let locked = config::UserConfig {
            theme: Some("Dracula".into()),
            system_apps_light: Some(false),
            ..Default::default()
        };
        let dracula = render::theme_by_name("Dracula").unwrap().bg;
        let mut app = app_for_theme(locked);
        app.on_system_theme(theme::SystemThemeEvent::Winit { light: true });
        assert_eq!(app.config.system_apps_light, Some(true));
        assert_eq!(
            app.renderer.as_ref().map(|r| r.bg),
            Some(dracula),
            "a named scheme is not replaced"
        );
    }

    #[test]
    fn the_cwd_argument_opens_a_local_folder_and_refuses_the_rest() {
        let os = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert_eq!(cli_start_dir(os(&[])), Ok(None));
        let tmp = std::env::temp_dir();
        let tmp = tmp.to_str().expect("temp dir is Unicode");
        let want = term::accept_local_cwd(tmp);
        assert!(want.is_some());
        for args in [vec!["-d", tmp], vec!["--cwd", tmp]] {
            assert_eq!(cli_start_dir(os(&args)), Ok(want.clone()), "{args:?}");
        }
        let joined = format!("--cwd={tmp}");
        assert_eq!(cli_start_dir(os(&[&joined])), Ok(want.clone()));
        let here = std::env::current_dir().unwrap();
        assert_eq!(
            cli_start_dir(os(&["-d", "."])),
            Ok(term::accept_local_cwd(here.to_str().unwrap())),
            "a relative folder is taken from the current one"
        );
        // UNC and device forms are refused before anything is opened
        for bad in [
            r"\\server\share",
            "//server/share",
            r"\\?\C:\Windows",
            r"C:\no\such\bayan-cwd-arg",
        ] {
            assert!(cli_start_dir(os(&["-d", bad])).is_err(), "{bad}");
        }
        assert!(cli_start_dir(os(&["-d"])).is_err(), "a flag with no folder");
        assert!(cli_start_dir(os(&["--bogus"])).is_err(), "an unknown flag");
    }

    #[test]
    fn confirm_cards_send_only_into_the_pane_they_were_raised_for() {
        // R-02
        assert!(card_still_targets(Some(7), Some(7)));
        assert!(!card_still_targets(Some(7), Some(8)), "tab switched");
        assert!(!card_still_targets(Some(7), None), "pane gone");
        assert!(!card_still_targets(None, None));
        // R-01: the card says when a paste fans out
        assert_eq!(broadcast_note(0), None);
        assert_eq!(broadcast_note(1).as_deref(), Some(" — يُرسل إلى لوحتين"));
        assert_eq!(broadcast_note(2).as_deref(), Some(" — يُرسل إلى 3 لوحات"));
        assert_eq!(broadcast_note(10).as_deref(), Some(" — يُرسل إلى 11 لوحة"));
    }

    #[test]
    fn a_shortcut_edit_does_not_copy_lua_bindings_into_json() {
        use std::collections::HashMap;
        let mut saved = Some(HashMap::from([(
            "copy".to_string(),
            "ctrl+shift+c".to_string(),
        )]));
        let mut runtime = Some(HashMap::from([
            ("copy".to_string(), "ctrl+shift+c".to_string()),
            ("new-tab".to_string(), "ctrl+alt+t".to_string()),
        ]));
        apply_shortcut(
            &mut saved,
            &mut runtime,
            "copy",
            Some("ctrl+shift+x".into()),
        );
        assert_eq!(
            saved.as_ref().unwrap().get("copy").map(String::as_str),
            Some("ctrl+shift+x")
        );
        assert!(
            saved.as_ref().unwrap().get("new-tab").is_none(),
            "a lua binding must not be written into config.json: {saved:?}"
        );
        assert_eq!(
            runtime.as_ref().unwrap().get("new-tab").map(String::as_str),
            Some("ctrl+alt+t")
        );
        apply_shortcut(&mut saved, &mut runtime, "copy", None);
        assert!(
            !saved.as_ref().is_some_and(|m| m.contains_key("new-tab")),
            "clearing a shortcut must not copy the lua map: {saved:?}"
        );
        assert!(runtime.as_ref().unwrap().contains_key("new-tab"));
    }

    #[test]
    fn a_long_approval_card_scrolls_and_enter_waits() {
        let rows: Vec<String> = (0..40).map(|i| format!("row {i}")).collect();
        let (window, scroll, at_end) = lua_card_window(&rows, 0);
        assert_eq!(window.len(), LUA_CARD_ROWS);
        assert_eq!(scroll, 0);
        assert!(!at_end);
        assert_eq!(window[0], "row 0");
        let (end, _, at_end) = lua_card_window(&rows, 1000);
        assert!(at_end);
        assert_eq!(end.last().map(String::as_str), Some("row 39"));
        let short = vec!["only".to_string()];
        assert!(lua_card_window(&short, 0).2);

        let now = std::time::Instant::now();
        assert!(lua_enter_ready(None, now));
        assert!(!lua_enter_ready(Some(now + LUA_APPROVE_DELAY), now));
        assert!(lua_enter_ready(
            Some(now - std::time::Duration::from_millis(1)),
            now
        ));
    }

    #[test]
    fn a_confirm_card_goes_away_with_its_pane() {
        // The card is keyed by pane id (R-02 keeps the send safe). When that
        // pane is removed — shell exit, pane close, or the whole tab — the
        // card itself has to go, or it stays painted over another tab and
        // Enter does nothing.
        let card = |pane| {
            Some(PendingPaste {
                kind: PendingKind::Command,
                pane,
                text: "echo hi\r".into(),
                lines: 1,
                chars: 8,
                reason: "test".into(),
                lua: None,
                scroll: 0,
                saw_end: true,
            })
        };
        let mut pending = card(Some(7));
        dismiss_paste_for(&mut pending, &[7]);
        assert!(pending.is_none(), "the pane the card was raised for");
        let mut pending = card(Some(7));
        dismiss_paste_for(&mut pending, &[7, 8]);
        assert!(pending.is_none(), "a tab close removes every pane in it");
        let mut pending = card(Some(7));
        dismiss_paste_for(&mut pending, &[8]);
        assert!(pending.is_some(), "a neighbour closing leaves this card up");
        let mut pending = card(None);
        dismiss_paste_for(&mut pending, &[7]);
        assert!(pending.is_some(), "a card with no pane id matches nothing");
        // a paste card is the same map: the pane id, not the kind
        let mut pending = card(Some(3));
        pending.as_mut().unwrap().kind = PendingKind::Paste;
        dismiss_paste_for(&mut pending, &[3]);
        assert!(pending.is_none());
    }

    #[test]
    fn a_plugin_card_shows_every_char_it_will_type() {
        // R-03: the whole payload, CRs visible, wrapped into card rows
        let send = format!("echo safe\r{}\rrm -rf x\r", "x".repeat(120));
        let rows = card_preview(PendingKind::Command, &send);
        let shown: String = rows.concat();
        assert!(shown.contains("rm -rf x"), "{shown}");
        assert_eq!(
            shown.matches('\u{240D}').count(),
            3,
            "every CR is shown as ␍"
        );
        assert!(rows.iter().all(|r| r.chars().count() <= CARD_ROW_CHARS));
        assert!(!shown.contains('…'), "nothing elided");
        // the cap keeps it to ten rows
        let max = "y".repeat(plugins::ACTION_SEND_MAX);
        assert!(card_preview(PendingKind::Command, &max).len() <= 10);
        // a paste keeps its one elided row
        assert_eq!(card_preview(PendingKind::Paste, &max).len(), 1);
    }

    #[test]
    fn plugin_actions_always_wait_for_the_confirm_card() {
        // BAYAN-007: even a one-word payload is held, and the card names it
        let p = plugin_pending("Build", "cargo build\r", Some(1));
        assert_eq!(p.text, "cargo build\r");
        assert_eq!(
            p.kind,
            PendingKind::Command,
            "a command, not a bracketed paste"
        );
        assert_eq!(p.pane, Some(1));
        assert!(p.reason.contains("Build"), "{}", p.reason);
        let p = plugin_pending("ls", "ls", Some(1));
        assert_eq!((p.lines, p.chars), (1, 2));
        // and its palette row is badged
        assert!(plugin_row_label("لصق").starts_with("إضافة — "));
    }

    #[test]
    fn chrome_text_shows_spoofing_characters_as_markers() {
        // BAYAN-012
        let shown = chrome_text("rm -rf \u{202E}txt.exe");
        assert!(!shown.contains('\u{202E}'), "{shown}");
        assert!(shown.contains("<RLO>"), "{shown}");
        let long = "x".repeat(1000);
        assert!(chrome_text(&long).chars().count() <= CHROME_TEXT_MAX + 1);
    }

    #[test]
    fn posh_previews_render_the_selected_theme_first() {
        // BAYAN-006
        // R-04: eager rows all run; a burst of highlights runs only the last
        let r = |i: usize, eager: bool| (i, format!("t{i}"), eager);
        let (got, dropped) = collapse_posh_requests(vec![
            r(3, true),
            r(1, true),
            r(4, false),
            r(5, false),
            r(6, false),
        ]);
        let got: Vec<usize> = got.into_iter().map(|x| x.0).collect();
        assert_eq!(got, vec![3, 1, 6]);
        assert_eq!(dropped, vec![4, 5]);
        let (empty, none) = collapse_posh_requests(Vec::new());
        assert!(empty.is_empty());
        assert!(none.is_empty());
        // the same row kept later in the burst is not reported as dropped
        let (kept, dropped) = collapse_posh_requests(vec![
            (1, "t1".into(), false),
            (2, "t2".into(), false),
            (1, "t1".into(), false),
        ]);
        assert_eq!(
            kept.into_iter().map(|row| row.0).collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(dropped, vec![2]);
        // a highlight asks once; a second pass while it is in flight does not
        let mut asked = vec![false, false];
        assert!(claim_posh_preview(&mut asked, false, 1));
        assert!(!claim_posh_preview(&mut asked, false, 1));
        assert!(claim_posh_preview(&mut asked, false, 0));
        assert!(!claim_posh_preview(&mut asked, true, 0));
    }

    #[test]
    fn a_skipped_highlight_can_be_asked_again() {
        // B-1: highlight 1, then 2, then 3 in one burst. Collapse keeps only
        // 3. Coming back to 1 must request a preview; 3 stays in flight.
        let mut asked = vec![false; 4];
        let mut burst = Vec::new();
        for i in 1..=3 {
            assert!(claim_posh_preview(&mut asked, false, i), "highlight {i}");
            burst.push((i, format!("t{i}"), false));
        }
        assert!(
            !claim_posh_preview(&mut asked, false, 1),
            "the claim is held until the worker says the row did not run"
        );
        let (kept, dropped) = collapse_posh_requests(burst);
        assert_eq!(
            kept.into_iter().map(|row| row.0).collect::<Vec<_>>(),
            vec![3]
        );
        assert_eq!(dropped, vec![1, 2]);
        let arrived = vec![false; asked.len()];
        assert!(
            !release_dropped_posh(&mut asked, &arrived, &dropped, 3),
            "the row the burst kept needs nothing new"
        );
        assert!(
            claim_posh_preview(&mut asked, false, 1),
            "returning to a skipped row requests its preview"
        );
        assert!(
            !claim_posh_preview(&mut asked, false, 3),
            "the highlight still in flight is not started again"
        );
        // already back on the skipped row when the notice arrives
        let mut asked = vec![false, true, true, true];
        assert!(release_dropped_posh(&mut asked, &arrived, &dropped, 1));
        assert!(!asked[1]);
        assert!(asked[3]);
        // a preview that did arrive is not forgotten
        let mut asked = vec![true];
        assert!(!release_dropped_posh(&mut asked, &[true], &[0], 0));
        assert!(asked[0]);
    }

    #[test]
    fn restored_sessions_are_capped() {
        // BAYAN-014: a sane bound, not "every tab in the file"
        const { assert!(MAX_RESTORED_TABS >= 4 && MAX_RESTORED_TABS <= 32) };
        const { assert!(MAX_PANES_PER_TAB == 4) };
    }

    #[test]
    fn a_failed_first_shell_does_not_rewrite_the_previous_tab() {
        // N-05: spawn failure must not lend this saved tab to the one before it
        let failed = plan_restored_tab(false, 4, 3);
        assert!(!failed.apply);
        assert_eq!(failed.extra, 0);
        // and a tab that did start still stops at the same cap as split()
        let full = plan_restored_tab(true, MAX_PANES_PER_TAB, 5);
        assert!(full.apply);
        assert_eq!(full.extra, 0);
        let fresh = plan_restored_tab(true, 1, 100);
        assert!(fresh.apply);
        assert_eq!(fresh.extra, MAX_PANES_PER_TAB - 1);
        assert_eq!(plan_restored_tab(true, 2, 3).extra, 2);
        assert_eq!(plan_restored_tab(true, 0, 2).extra, 2);
    }

    #[test]
    fn meta_locks_are_not_unwrapped() {
        // BAYAN-015: `.lock()` then `.unwrap()` panics the UI when the
        // reader thread poisoned the mutex. The call is often split across
        // lines, which a single-line search misses.
        let files = [
            ("main.rs", include_str!("main.rs")),
            ("term.rs", include_str!("term.rs")),
            ("input.rs", include_str!("input.rs")),
            ("keyroute.rs", include_str!("keyroute.rs")),
        ];
        for (name, src) in files {
            let hits = unwrap_after_lock(&strip_line_comments(src));
            assert!(
                hits.is_empty(),
                "{name}: lock().unwrap at lines {hits:?} — use lock_or_recover"
            );
        }
    }

    #[test]
    fn search_seed_is_bounded_literal_and_restores_claude_arabic() {
        assert_eq!(selection_search_query(Some("a.b(c)"), false), "a.b(c)");
        assert_eq!(selection_search_query(Some("ابحرم"), true), "مرحبا");
        for text in [
            "one\ntwo".to_string(),
            "one\rtwo".to_string(),
            "x".repeat(257),
        ] {
            assert!(selection_search_query(Some(&text), false).is_empty());
        }
        assert!(selection_search_query(None, false).is_empty());
        let seed = SearchState::from_selection(Some("ابحرم"), true);
        assert_eq!(seed.query, "مرحبا");
        let size = TermSize { cols: 40, rows: 3 };
        let mut t = alacritty_terminal::term::Term::new(
            alacritty_terminal::term::Config::default(),
            &size,
            alacritty_terminal::event::VoidListener,
        );
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut t, "ابحرم\r\n".as_bytes());
        assert!(
            find_match(
                &mut t,
                seed.selected_needle.as_deref().unwrap(),
                &None,
                Direction::Left
            )
            .is_some(),
            "the readable seed must still search Claude's source cells"
        );
    }

    #[test]
    fn keyboard_zoom_uses_physical_keys_and_leaves_altgr_alone() {
        use winit::keyboard::{KeyCode, PhysicalKey};
        for code in [KeyCode::Equal, KeyCode::NumpadAdd] {
            assert_eq!(
                keyboard_zoom(PhysicalKey::Code(code), true, false, true),
                Some(1)
            );
            assert_eq!(
                keyboard_zoom(PhysicalKey::Code(code), true, true, false),
                None
            );
        }
        assert_eq!(
            keyboard_zoom(PhysicalKey::Code(KeyCode::Minus), true, false, false),
            Some(-1)
        );
        assert_eq!(
            keyboard_zoom(PhysicalKey::Code(KeyCode::Minus), true, false, true),
            None
        );
        assert_eq!(
            keyboard_zoom(
                PhysicalKey::Code(KeyCode::NumpadSubtract),
                false,
                false,
                false
            ),
            None
        );
    }

    #[test]
    fn palette_profiles_and_copy_targets_keep_exact_payloads() {
        let shells = vec![shells::Shell {
            name: "طرفية تجريبية".into(),
            command: "cmd.exe".into(),
        }];
        let spans = vec![link::LinkSpan {
            start_col: 2,
            end_col: 20,
            kind: link::LinkKind::Url {
                uri: "https://example.org/ملف".into(),
            },
        }];
        let rows = palette_extras(&shells, &spans);
        assert!(rows.iter().any(
            |(r, label)| matches!(r, PaletteRow::Shell(cmd) if cmd == "cmd.exe")
                && label.contains("طرفية تجريبية")
        ));
        assert!(rows.iter().any(
            |(r, _)| matches!(r, PaletteRow::CopyText(text) if text == "https://example.org/ملف")
        ));
    }

    #[test]
    fn palette_copy_list_is_bounded_deduplicated_and_has_safe_labels() {
        let span = link::LinkSpan {
            start_col: 0,
            end_col: 9,
            kind: link::LinkKind::Osc {
                uri: "https://example.org/\u{202e}x".into(),
                shown: "x".into(),
            },
        };
        let rows = palette_extras(&[], &vec![span; 200]);
        let copies: Vec<_> = rows
            .iter()
            .filter(|(r, _)| matches!(r, PaletteRow::CopyText(_)))
            .collect();
        assert_eq!(copies.len(), 1);
        assert!(!copies[0].1.contains('\u{202e}'));
        let spans: Vec<_> = (0..200)
            .map(|n| link::LinkSpan {
                start_col: 0,
                end_col: 9,
                kind: link::LinkKind::File {
                    path: format!("file{n}.rs"),
                    line: 0,
                    col: None,
                },
            })
            .collect();
        let rows = palette_extras(&[], &spans);
        assert_eq!(
            rows.iter()
                .filter(|(r, _)| matches!(r, PaletteRow::CopyText(_)))
                .count(),
            100
        );
    }

    #[test]
    fn dismissing_guard_consumes_its_release_even_after_the_modal_is_gone() {
        let mut pending = true;
        assert!(!consume_guard_release(
            &mut pending,
            ElementState::Released,
            MouseButton::Right
        ));
        assert!(pending);
        assert!(consume_guard_release(
            &mut pending,
            ElementState::Released,
            MouseButton::Left
        ));
        assert!(!pending);
        assert!(!consume_guard_release(
            &mut pending,
            ElementState::Released,
            MouseButton::Left
        ));
    }

    #[test]
    fn a_new_press_cancels_a_release_left_over_from_a_dismissed_modal() {
        let mut pending = true;
        assert!(!consume_guard_release(
            &mut pending,
            ElementState::Pressed,
            MouseButton::Left
        ));
        assert!(
            !pending,
            "a new press must not inherit the old modal's release suppression"
        );
        assert!(!consume_guard_release(
            &mut pending,
            ElementState::Released,
            MouseButton::Left
        ));
    }

    #[test]
    fn window_close_replaces_pane_confirmation_and_preserves_window_guard() {
        let mut pending = Some(CloseTarget::Pane(0, 0));
        assert!(!prepare_window_close(&mut pending, true, 2));
        assert!(matches!(pending, Some(CloseTarget::Window)));
        assert!(!prepare_window_close(&mut pending, true, 2));

        let mut pending = None;
        assert!(!prepare_window_close(&mut pending, true, 1));
        assert!(matches!(pending, Some(CloseTarget::Window)));

        for (confirm, running) in [(false, 2), (true, 0)] {
            let mut pending = Some(CloseTarget::Pane(0, 0));
            assert!(prepare_window_close(&mut pending, confirm, running));
            assert!(pending.is_none());
        }
    }

    #[test]
    fn blur_drops_keys_whose_press_was_swallowed() {
        use winit::keyboard::{KeyCode, PhysicalKey};
        let key = PhysicalKey::Code(KeyCode::KeyC);
        let mut held = vec![key];
        blur_forgets_held_keys(false, &mut held);
        assert!(held.is_empty());
        held.push(key);
        blur_forgets_held_keys(true, &mut held);
        assert_eq!(held, vec![key]);
    }

    #[test]
    fn settings_step_from_the_nearest_preset_and_stop_at_the_ends() {
        let steps = [0, 4, 8, 12];
        assert_eq!(step_preset(&steps, 4, 1), 8);
        assert_eq!(step_preset(&steps, 5, -1), 4); // 5 snaps to 8, then down
        assert_eq!(step_preset(&steps, 12, 1), 12);
        assert_eq!(step_preset(&steps, 0, -1), 0);
        assert_eq!(step_preset(&steps, 99, -1), 8); // past the end: last, then down
    }

    #[test]
    fn osc52_pane_is_focused() {
        // active tab's focused pane is index 1 (id 20). Id 10 is the other split.
        let tabs: [(usize, &[u64]); 2] = [(1, &[10, 20]), (0, &[30])];
        assert!(super::osc52_pane_is_focused(&tabs, 0, 20));
        assert!(
            !super::osc52_pane_is_focused(&tabs, 0, 10),
            "unfocused split"
        );
        assert!(
            !super::osc52_pane_is_focused(&tabs, 0, 30),
            "background tab"
        );
        assert!(!super::osc52_pane_is_focused(&tabs, 5, 20), "no such tab");
        let past_end: [(usize, &[u64]); 1] = [(3, &[1])];
        assert!(
            !super::osc52_pane_is_focused(&past_end, 0, 1),
            "focused index past the end"
        );
    }

    #[test]
    fn menu_rows_show_their_live_shortcut() {
        let cfg = config::UserConfig::default();
        let map = keybinds::effective_map(&cfg);
        let split = row_shortcut(&map, &PaletteRow::Builtin(keybinds::Action::SplitH));
        assert_eq!(split, keybinds::Action::SplitH.default_chord().display());
        assert!(split.contains("Ctrl"), "{split}");
        // rows that are not actions have no shortcut
        assert_eq!(row_shortcut(&map, &PaletteRow::CopyText("x".into())), "");
        // a rebinding shows up in the menu
        let mut binds = std::collections::HashMap::new();
        binds.insert("split-h".to_string(), "ctrl+alt+h".to_string());
        let cfg = config::UserConfig {
            keybinds: Some(binds),
            ..Default::default()
        };
        let map = keybinds::effective_map(&cfg);
        assert_eq!(
            row_shortcut(&map, &PaletteRow::Builtin(keybinds::Action::SplitH)),
            keybinds::Chord::parse("ctrl+alt+h").unwrap().display()
        );
    }

    #[test]
    fn the_context_menu_uses_the_action_labels() {
        let rows = context_menu_rows(true, 2, false, false);
        let label = |a: keybinds::Action| {
            rows.iter()
                .find(|(r, _)| r == &PaletteRow::Builtin(a))
                .map(|(_, s)| s.as_str())
                .unwrap()
        };
        assert_eq!(
            label(keybinds::Action::Paste),
            keybinds::Action::Paste.label()
        );
        assert_eq!(
            label(keybinds::Action::Copy),
            keybinds::Action::Copy.label()
        );
        assert_eq!(
            label(keybinds::Action::SplitV),
            keybinds::Action::SplitV.label()
        );
        assert_eq!(label(keybinds::Action::SplitV), "لوحة أسفل");
        assert_eq!(
            label(keybinds::Action::ZoomPane),
            keybinds::Action::ZoomPane.label()
        );
        let zoomed = context_menu_rows(false, 2, true, true);
        let zoom = zoomed
            .iter()
            .find(|(r, _)| r == &PaletteRow::Builtin(keybinds::Action::ZoomPane))
            .map(|(_, s)| s.as_str())
            .unwrap();
        assert!(zoom.starts_with("إلغاء "), "{zoom}");
        let cast = zoomed
            .iter()
            .find(|(r, _)| r == &PaletteRow::Builtin(keybinds::Action::Broadcast))
            .map(|(_, s)| s.as_str())
            .unwrap();
        assert!(cast.starts_with("إيقاف "), "{cast}");
    }

    #[test]
    fn a_saved_tree_restores_and_an_old_file_still_reads() {
        let mut tree = layout::Node::single();
        tree.split(0, Orientation::Row, 1);
        tree.split(1, Orientation::Column, 2);
        let st = SavedTab {
            cwds: vec![None, None, None],
            weights: Vec::new(),
            orientation: 0,
            focused: 0,
            layout: Some(tree.clone()),
            name: None,
            group: 0,
            shells: Vec::new(),
        };
        assert_eq!(restored_layout(&st, 3), tree);
        let (orientation, weights) = saved_split(&tree);
        assert_eq!(orientation, 0, "the root split is side by side");
        assert_eq!(weights.len(), 2);
        // a pane failed to respawn and the file kept the root axis: that
        // axis is the fallback, not a hard-coded row of equal panes
        let fallback = SavedTab {
            cwds: st.cwds.clone(),
            weights: weights.clone(),
            orientation,
            focused: st.focused,
            layout: Some(tree.clone()),
            name: st.name.clone(),
            group: 0,
            shells: Vec::new(),
        };
        assert_eq!(
            restored_layout(&fallback, 2),
            layout::Node::flat(Orientation::Row, &weights, 2)
        );
        // a save that dropped the axis (the old writer) still reads as a row
        assert_eq!(
            restored_layout(&st, 2),
            layout::Node::flat(Orientation::Row, &[], 2)
        );
        // a file from before nested splits: orientation + weights
        let old: SavedTab =
            serde_json::from_str(r#"{"cwds":[null,null],"weights":[3.0,1.0],"orientation":1}"#)
                .unwrap();
        assert_eq!(
            restored_layout(&old, 2),
            layout::Node::flat(Orientation::Column, &[3.0, 1.0], 2)
        );
        // and the split labels say which way they go
        assert_ne!(
            keybinds::Action::SplitH.label(),
            keybinds::Action::SplitV.label()
        );
    }

    #[test]
    fn jump_offset_walks_prompts() {
        // prompts at absolute lines 10, 40, 80; history 100, at the bottom
        let marks = [10usize, 40, 80];
        // viewport top = 100: previous prompt is 80 -> off = 100-80+1 = 21
        assert_eq!(jump_offset(&marks, 100, 0, -1), Some(21));
        // from there (top=79), previous is 40 -> off = 61
        assert_eq!(jump_offset(&marks, 100, 21, -1), Some(61));
        // and next from top=39 is 40 -> off = 61? no: 40 > 39 -> off = 100-40+1
        assert_eq!(jump_offset(&marks, 100, 61, 1), Some(61));
        // nothing above the very first prompt
        assert_eq!(jump_offset(&marks, 100, 91, -1), None);
        // no marks, no jump
        assert_eq!(jump_offset(&[], 100, 0, -1), None);
    }

    /// Command marks accumulate with exit codes through the OSC 133 stream.
    #[test]
    fn command_marks_record_prompts_and_exits() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;
        let size = TermSize { cols: 20, rows: 4 };
        let mut t = Term::new(Config::default(), &size, VoidListener);
        let mut p: Processor = Processor::new();
        let mut meta = term::SessionMeta {
            prompt_nonce: "n0nce".into(),
            ..term::SessionMeta::default()
        };
        let mut carry = Vec::new();
        let feed = |p: &mut Processor,
                    t: &mut Term<VoidListener>,
                    m: &mut term::SessionMeta,
                    c: &mut Vec<u8>,
                    d: &[u8]| {
            term::process_chunk(p, t, m, c, d);
        };
        feed(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]133;A;n0nce\x07> ok\r\n",
        );
        feed(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]133;D;0;n0nce\x07",
        );
        feed(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]133;A;n0nce\x07> bad\r\n",
        );
        feed(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]133;D;1;n0nce\x07",
        );
        assert_eq!(meta.marks.len(), 2);
        assert_eq!(meta.marks[0].exit, Some(0));
        assert_eq!(meta.marks[1].exit, Some(1));
        assert!(meta.marks[1].abs > meta.marks[0].abs);
    }

    #[test]
    fn window_geometry_restores_only_onto_a_live_monitor() {
        let primary = (0, 0, 2560u32, 1440u32);
        let second = (2560, 0, 1920u32, 1080u32);
        let g = |x, y| WindowGeom {
            x,
            y,
            w: 1100,
            h: 700,
            maximized: false,
        };
        assert!(geometry_on_screen(g(100, 100), &[primary]));
        // on the second monitor: fine while it is connected, not after
        assert!(geometry_on_screen(g(3000, 200), &[primary, second]));
        assert!(!geometry_on_screen(g(3000, 200), &[primary]));
        // mostly off the right edge, but 160px of the grab strip still reaches in
        assert!(geometry_on_screen(g(2400, 100), &[primary]));
        // only 60px in: not enough to grab — treat as off-screen
        assert!(!geometry_on_screen(g(2500, 100), &[primary]));
        // above the top: the title strip is unreachable
        assert!(!geometry_on_screen(g(100, -400), &[primary]));
        // a degenerate size is not worth restoring
        assert!(!geometry_on_screen(
            WindowGeom {
                w: 10,
                h: 10,
                ..g(0, 0)
            },
            &[primary]
        ));
        assert!(!geometry_on_screen(g(0, 0), &[]));
    }

    #[test]
    fn saved_state_round_trips_with_layout() {
        let s = SavedState {
            tabs: vec![SavedTab {
                cwds: vec![Some(r"C:\Users\Admin\Bayan".into()), None],
                weights: vec![3.0, 1.0],
                orientation: 1,
                focused: 1,
                layout: None,
                name: Some("خادم".into()),
                group: 0,
                shells: Vec::new(),
            }],
            active: 0,
            groups: Vec::new(),
            window: Some(WindowGeom {
                x: 40,
                y: 60,
                w: 1280,
                h: 800,
                maximized: true,
            }),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back = parse_state(&json).unwrap();
        assert_eq!(back.window, s.window, "geometry survives the round trip");
        // a pre-M34 file has no window field and must still parse
        let old = r#"{"tabs":[{"cwds":[null]}],"active":0}"#;
        assert_eq!(parse_state(old).unwrap().window, None);
        assert_eq!(back.tabs[0].cwds.len(), 2);
        assert_eq!(back.tabs[0].weights, vec![3.0, 1.0]);
        assert_eq!(back.tabs[0].orientation, 1);
        assert_eq!(back.tabs[0].focused, 1);
        // the pre-M9 format (one cwd per tab) still restores
        let legacy = r#"{"tabs":["C:\\x","C:\\y"],"active":1}"#;
        let back = parse_state(legacy).unwrap();
        assert_eq!(back.tabs.len(), 2);
        assert_eq!(back.tabs[0].cwds[0].as_deref(), Some(r"C:\x"));
        assert_eq!(back.active, 1);
        // a corrupt file must not crash restore (it falls back to one tab)
        assert!(parse_state("{broken").is_none());
    }

    #[test]
    fn ctrl_letter_decodes_windows_control_chars() {
        assert_eq!(ctrl_letter(&Key::Character("\u{6}".into())), Some('f')); // Ctrl+F
        assert_eq!(ctrl_letter(&Key::Character("\u{3}".into())), Some('c')); // Ctrl+C
        assert_eq!(ctrl_letter(&Key::Character("\u{16}".into())), Some('v')); // Ctrl+V
        assert_eq!(ctrl_letter(&Key::Character("F".into())), Some('f'));
        assert_eq!(ctrl_letter(&Key::Character("7".into())), None);
        assert_eq!(ctrl_letter(&Key::Named(NamedKey::Enter)), None);
    }

    #[test]
    fn click_tracker_counts_like_a_terminal() {
        let t0 = Instant::now();
        let mut c = ClickTracker::new();
        assert_eq!(c.click(t0, (5, 10)), 1);
        assert_eq!(c.click(t0 + Duration::from_millis(150), (5, 10)), 2); // word
        assert_eq!(c.click(t0 + Duration::from_millis(300), (5, 10)), 3); // line
                                                                          // a 4th quick click starts over
        assert_eq!(c.click(t0 + Duration::from_millis(450), (5, 10)), 1);
        // too slow -> single again
        assert_eq!(c.click(t0 + Duration::from_millis(2000), (5, 10)), 1);
        // different cell -> single
        assert_eq!(c.click(t0 + Duration::from_millis(2100), (9, 9)), 1);
    }

    /// Exercise the search core against a REAL emulator with real content —
    /// the GUI-free equivalent of Ctrl+F, Enter, Enter, Shift+Enter.
    #[test]
    fn search_finds_steps_and_wraps_on_a_real_term() {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;

        let size = TermSize { cols: 40, rows: 6 };
        let mut t = Term::new(Config::default(), &size, VoidListener);
        let mut p: Processor = Processor::new();
        for line in ["alpha needle one", "plain line", "needle two", "tail"] {
            p.advance(&mut t, line.as_bytes());
            p.advance(&mut t, b"\r\n");
        }

        // Enter: nearest match above the bottom = "needle two" (row 2)
        let m1 = find_match(&mut t, "needle", &None, Direction::Left).expect("first match");
        assert_eq!(m1.start().line.0, 2);
        // Enter again: steps up to "needle one" (row 0)
        let m2 =
            find_match(&mut t, "needle", &Some(m1.clone()), Direction::Left).expect("second match");
        assert_eq!(m2.start().line.0, 0);
        // Enter once more: wraps around back to row 2
        let m3 = find_match(&mut t, "needle", &Some(m2.clone()), Direction::Left)
            .expect("wrapped match");
        assert_eq!(m3.start().line.0, 2);
        // Shift+Enter from the first hit walks back down
        let m4 = find_match(&mut t, "needle", &Some(m2), Direction::Right).expect("forward match");
        assert_eq!(m4.start().line.0, 2);
        // no hits -> None, and a regex-special query is treated literally
        assert!(find_match(&mut t, "absent", &None, Direction::Left).is_none());
        assert!(find_match(&mut t, "needle (", &None, Direction::Left).is_none());
    }

    /// A background tab's shell exiting must not move focus to a different
    /// tab than the one the user was looking at.
    #[test]
    fn closing_a_lower_tab_slides_the_active_index_down() {
        // 4 tabs, looking at #2, tab #0 closes -> the same tab is now #1
        // and it is still the tab you see, so its search bar stays
        assert_eq!(active_after_remove(2, 0, 3), (1, false));
        // closing a higher tab leaves the index alone
        assert_eq!(active_after_remove(1, 3, 3), (1, false));
        // closing the active tab itself: clamp to the last remaining, and
        // the tab now shown is a different one (its search bar must go)
        assert_eq!(active_after_remove(2, 2, 2), (1, true));
        assert_eq!(active_after_remove(0, 0, 2), (0, true));
        assert_eq!(active_after_remove(5, 0, 0), (0, false));
    }

    /// The toast trigger is untrusted output: a program can ask for
    /// attention in a loop, and one balloon per read chunk buries the
    /// notification centre under text Bayan appears to have written.
    #[test]
    fn pty_toasts_are_rate_limited() {
        let gap = std::time::Duration::from_secs(5);
        let t0 = Instant::now();
        assert!(toast_due(None, t0, gap), "the first one always fires");
        assert!(
            !toast_due(Some(t0), t0, gap),
            "a second one in the same instant does not"
        );
        assert!(!toast_due(
            Some(t0),
            t0 + std::time::Duration::from_millis(4999),
            gap
        ));
        assert!(
            toast_due(Some(t0), t0 + gap, gap),
            "exactly at the gap it fires again"
        );
        assert!(toast_due(
            Some(t0),
            t0 + std::time::Duration::from_secs(60),
            gap
        ));
    }

    #[test]
    fn step_point_walks_and_clamps() {
        let (cols, top, bottom) = (10, -100, 31);
        let p = Point::new(Line(0), Column(9));
        assert_eq!(
            step_point(p, cols, top, bottom, Direction::Right),
            Point::new(Line(1), Column(0))
        );
        let q = Point::new(Line(0), Column(0));
        assert_eq!(
            step_point(q, cols, top, bottom, Direction::Left),
            Point::new(Line(-1), Column(9))
        );
        // clamped at the extremes
        let end = Point::new(Line(bottom), Column(9));
        assert_eq!(step_point(end, cols, top, bottom, Direction::Right), end);
        let start = Point::new(Line(top), Column(0));
        assert_eq!(step_point(start, cols, top, bottom, Direction::Left), start);
    }
}

/// Drop `//` comments so a doc line that names the old call does not look
/// like a live one. Strings are left as they are; none of the scanned files
/// put this call inside a string.
#[cfg(test)]
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        match line.find("//") {
            Some(i) => out.push_str(&line[..i]),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Lines where `.lock()` is followed only by whitespace and then `.unwrap()`.
/// The needles are built in pieces so this scanner does not flag itself.
#[cfg(test)]
fn unwrap_after_lock(src: &str) -> Vec<usize> {
    let lock = concat!(".", "lock()");
    let unwrap = concat!(".", "unwrap()");
    let mut hits = Vec::new();
    let mut offset = 0;
    let mut line = 1usize;
    while let Some(rel) = src[offset..].find(lock) {
        let at = offset + rel;
        line += src[offset..at].bytes().filter(|&b| b == b'\n').count();
        let after = &src[at + lock.len()..];
        if after.trim_start().starts_with(unwrap) {
            hits.push(line);
        }
        let next = at + lock.len();
        line += src[at..next].bytes().filter(|&b| b == b'\n').count();
        offset = next;
    }
    hits
}

#[cfg(test)]
mod palette_tests {
    use super::*;

    #[test]
    fn the_hint_line_counts_what_the_list_shows() {
        // The hint prints the length of the list Enter indexes into. That
        // list is Action::ALL (plugins add to it at runtime). A literal
        // count goes stale the moment an action is added.
        let n = keybinds::Action::ALL.len();
        assert_eq!(
            render::palette_hint(n),
            format!("{n} نتيجة · Enter تشغيل · ↑↓ تنقّل · Esc إغلاق"),
        );
        let extra = n + 1;
        assert_eq!(
            render::palette_hint(extra),
            format!("{extra} نتيجة · Enter تشغيل · ↑↓ تنقّل · Esc إغلاق"),
        );
        assert_eq!(render::palette_hint(0), "لا نتائج · Esc إغلاق");
    }
}

#[cfg(test)]
mod m38_tests {
    use super::*;

    #[test]
    fn tab_names_are_trimmed_capped_and_stripped_of_bidi_controls() {
        assert_eq!(clean_tab_name("  خادم الويب  "), Some("خادم الويب".into()));
        assert_eq!(clean_tab_name(""), None);
        assert_eq!(clean_tab_name("   "), None);
        // an override must not be able to reorder the tab bar around it
        assert_eq!(clean_tab_name("abc\u{202E}fed"), Some("abcfed".into()));
        assert_eq!(clean_tab_name("\u{2067}x\u{2069}"), Some("x".into()));
        assert_eq!(clean_tab_name("a\x1b[31mb\n"), Some("a[31mb".into()));
        let long = "ب".repeat(100);
        assert_eq!(clean_tab_name(&long).unwrap().chars().count(), TAB_NAME_MAX);
        // the digits stay digits: ١٧٥ is a name like any other
        assert_eq!(clean_tab_name("لوحة ١٧٥"), Some("لوحة ١٧٥".into()));
    }

    #[test]
    fn the_label_marks_broadcast_and_zoom() {
        assert_eq!(tab_label("Bayan", false, 0), "Bayan");
        assert_eq!(tab_label("Bayan", true, 0), "» Bayan");
        assert_eq!(tab_label("Bayan", false, 2), "Bayan +2");
        assert_eq!(tab_label("Bayan", true, 1), "» Bayan +1");
    }

    #[test]
    fn rename_rows_put_the_typed_name_first_and_always_offer_the_reset() {
        let rows = rename_rows(RenameTarget::Tab, " api ");
        assert_eq!(rows.len(), 2);
        assert!(rows[0].0 == PaletteRow::RenameTab(Some("api".into())));
        assert!(rows[1].0 == PaletteRow::RenameTab(None));
        // nothing typed: Enter restores the folder name
        let rows = rename_rows(RenameTarget::Tab, "");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].0 == PaletteRow::RenameTab(None));
        // the same shape for a group, through the same sanitizer
        let rows = rename_rows(RenameTarget::Group, " back\u{202E}end ");
        assert!(rows[0].0 == PaletteRow::RenameGroup(Some("backend".into())));
        assert!(rows[1].0 == PaletteRow::RenameGroup(None));
    }

    #[test]
    fn group_rows_offer_only_moves_that_change_something() {
        // one group: nothing to show or move into, only a new group
        let one = group_rows(&[(0, "مجموعة 1".into())], 0);
        assert_eq!(one.len(), 1);
        assert!(one[0].0 == PaletteRow::MoveToGroup(None));
        // three groups, the second on screen
        let g = [
            (0, "a".to_string()),
            (1, "b".to_string()),
            (2, "c".to_string()),
        ];
        let rows: Vec<PaletteRow> = group_rows(&g, 1).into_iter().map(|r| r.0).collect();
        assert!(
            rows == vec![
                PaletteRow::ShowGroup(0),
                PaletteRow::ShowGroup(2),
                PaletteRow::MoveToGroup(Some(0)),
                PaletteRow::MoveToGroup(Some(2)),
                PaletteRow::MoveToGroup(None),
                PaletteRow::OpenRenameGroup,
            ]
        );
    }

    #[test]
    fn each_panes_shell_round_trips_and_only_detectable_ones_come_back() {
        let installed = vec![shells::Shell {
            name: "PowerShell 7".into(),
            command: "pwsh.exe -NoLogo".into(),
        }];
        let def = "C:\\PROGRA~1\\Git\\bin\\bash.exe --login -i";
        // all default: the field stays out of the file
        assert!(saved_shells([def, def].into_iter(), def).is_empty());
        let shells = saved_shells(["cmd.exe", def, "pwsh.exe -NoLogo"].into_iter(), def);
        assert_eq!(
            shells,
            vec![
                Some("cmd.exe".into()),
                None,
                Some("pwsh.exe -NoLogo".into())
            ]
        );
        let st = SavedState {
            tabs: vec![SavedTab {
                cwds: vec![None, None, None],
                weights: Vec::new(),
                orientation: 0,
                focused: 0,
                layout: None,
                name: None,
                group: 0,
                shells,
            }],
            active: 0,
            groups: Vec::new(),
            window: None,
        };
        let back = parse_state(&serde_json::to_string(&st).unwrap()).unwrap();
        let got: Vec<String> = back.tabs[0]
            .shells
            .iter()
            .cloned()
            .map(|s| restored_shell(s, def, &installed))
            .collect();
        assert_eq!(got, vec!["cmd.exe", def, "pwsh.exe -NoLogo"]);
        // a file from before the field restores every pane on the default
        let old = parse_state(r#"{"tabs":[{"cwds":[null]}],"active":0}"#).unwrap();
        assert!(old.tabs[0].shells.is_empty());
        // a planted or hand-edited command is not run: the default is
        assert_eq!(
            restored_shell(Some("calc.exe".into()), def, &installed),
            def
        );
        assert_eq!(
            restored_shell(Some("cmd.exe /c del x".into()), def, &installed),
            def
        );
        assert_eq!(restored_shell(None, def, &installed), def);
    }

    #[test]
    fn removed_wsl_distribution_restores_the_default_shell() {
        assert_eq!(
            restored_shell(
                Some("wsl.exe -d Bayan-Removed-Distro".into()),
                "cmd.exe",
                &[]
            ),
            "cmd.exe"
        );
    }

    #[test]
    fn groups_round_trip_and_files_without_them_are_one_group() {
        let tab = |group| SavedTab {
            cwds: vec![None],
            weights: Vec::new(),
            orientation: 0,
            focused: 0,
            layout: None,
            name: None,
            group,
            shells: Vec::new(),
        };
        let s = SavedState {
            tabs: vec![tab(0), tab(1), tab(0)],
            active: 1,
            groups: vec![None, Some("docs".into())],
            window: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back = parse_state(&json).unwrap();
        let saved: Vec<usize> = back.tabs.iter().map(|t| t.group).collect();
        assert_eq!(saved, vec![0, 1, 0]);
        assert_eq!(back.groups, vec![None, Some("docs".to_string())]);
        // the first group's tabs write no `group` key at all
        assert_eq!(json.matches("\"group\"").count(), 1);
        // a file from before groups: every tab in the one group
        let old = parse_state(r#"{"tabs":[{"cwds":[null]},{"cwds":[null]}],"active":0}"#).unwrap();
        assert!(old.groups.is_empty());
        assert!(old.tabs.iter().all(|t| t.group == 0));
    }

    #[test]
    fn zoom_gives_the_focused_pane_everything_and_hides_the_rest() {
        let content = (10, 40, 1000, 600);
        let rects = zoom_rects(content, 3, 1);
        assert_eq!(
            rects.len(),
            3,
            "one rect per pane, so indices still line up"
        );
        assert_eq!(rects[1], content);
        assert!(!rect_visible(&rects[0]) && !rect_visible(&rects[2]));
        // a hidden rect is not a cell either
        assert_eq!(mouse::cell_at(15.0, 45.0, &[rects[0]], 8.0, 16.0), None);
        assert!(rect_visible(&content));
    }

    #[test]
    fn broadcast_reaches_every_other_pane_only_when_on() {
        assert!(broadcast_targets(false, 3, 0).is_empty());
        assert_eq!(broadcast_targets(true, 3, 1), vec![0, 2]);
        assert!(broadcast_targets(true, 1, 0).is_empty());
    }

    #[test]
    fn output_rows_map_to_grid_lines_and_drop_what_scrolled_away() {
        // 100 rows of history, nothing evicted: global row 100 is line 0
        assert_eq!(output_lines(100, 105, 0, 100, 30), Some((0, 4)));
        assert_eq!(output_lines(90, 95, 0, 100, 30), Some((-10, -6)));
        // 50 rows evicted past the cap: the same content moved up by 50
        assert_eq!(output_lines(150, 155, 50, 100, 30), Some((0, 4)));
        // still inside the scrollback: global row 10 is history line -30
        assert_eq!(output_lines(10, 60, 0, 40, 30), Some((-30, 19)));
        // 20 rows evicted: rows 0..20 are gone, so the block starts at the
        // top of what is left (line -40) instead of at line -50
        assert_eq!(output_lines(10, 80, 20, 40, 30), Some((-40, 19)));
        // wholly evicted, or empty
        assert_eq!(output_lines(0, 5, 20, 40, 30), None);
        assert_eq!(output_lines(7, 7, 0, 40, 30), None);
    }

    #[test]
    fn last_output_is_between_the_enter_and_the_next_prompt() {
        use term::CmdMark;
        let m = |abs, out| CmdMark {
            abs,
            exit: Some(0),
            out,
        };
        // prompt at 10, Enter on 10 -> output 11..20, prompt at 20 (idle)
        assert_eq!(
            term::last_output_range(&[m(10, Some(11)), m(20, None)]),
            Some((11, 20))
        );
        // an empty Enter at 20 printed nothing: the block before still counts
        let marks = [m(10, Some(11)), m(20, Some(21)), m(21, None)];
        assert_eq!(term::last_output_range(&marks), Some((11, 20)));
        // a wrapped command: Enter on the prompt's third row
        assert_eq!(
            term::last_output_range(&[m(5, Some(8)), m(12, None)]),
            Some((8, 12))
        );
        // no submitted command, or only the current prompt
        assert_eq!(term::last_output_range(&[m(3, None), m(9, None)]), None);
        assert_eq!(term::last_output_range(&[m(3, Some(4))]), None);
        assert_eq!(term::last_output_range(&[]), None);
    }

    #[test]
    fn the_arabic_font_cycler_rings_through_auto() {
        let c = vec!["Amiri".to_string(), "Cairo".to_string()];
        assert_eq!(step_arabic_font(&c, None, 1).as_deref(), Some("Amiri"));
        assert_eq!(
            step_arabic_font(&c, Some("Amiri"), 1).as_deref(),
            Some("Cairo")
        );
        assert_eq!(
            step_arabic_font(&c, Some("Cairo"), 1),
            None,
            "wraps to Auto"
        );
        assert_eq!(step_arabic_font(&c, None, -1).as_deref(), Some("Cairo"));
        assert_eq!(
            step_arabic_font(&c, Some("Gone"), 1).as_deref(),
            Some("Amiri")
        );
        assert_eq!(step_arabic_font(&[], None, 1), None, "nothing but Auto");
    }

    #[test]
    fn english_menus_and_cards_read_in_english() {
        lang::set(lang::Lang::En);
        let labels: Vec<String> = context_menu_rows(true, 2, false, false)
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert_eq!(&labels[..3], ["Copy selection", "Copy clean", "Paste"]);
        assert!(labels.iter().any(|l| l == "New tab with shell…"));
        let rows = shell_menu_rows(&[], "cmd.exe");
        assert_eq!(rows[1].1, "Command Prompt · default");
        assert_eq!(broadcast_note(2).as_deref(), Some(" — goes to 3 panes"));
        assert_eq!(keybinds::Action::NextGroup.label(), "Next group");
        // nothing in the shortcuts registry is left in Arabic
        for a in keybinds::Action::ALL {
            assert!(
                !a.label()
                    .chars()
                    .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
                "{} has no English label",
                a.id()
            );
        }
        lang::set(lang::Lang::Ar);
        assert_eq!(keybinds::Action::NextGroup.label(), "المجموعة التالية");
    }

    #[test]
    fn the_menu_opens_left_of_the_pointer_and_stays_inside_the_window() {
        let frame = (1000, 800);
        // room on the left and below: left of the pointer, top at the pointer
        assert_eq!(
            render::menu_origin((600.0, 300.0), (250, 400), frame),
            (350, 300)
        );
        // too close to the left edge: it opens to the right instead
        assert_eq!(
            render::menu_origin((100.0, 300.0), (250, 400), frame),
            (100, 300)
        );
        // too close to the bottom: it opens upward
        assert_eq!(
            render::menu_origin((600.0, 700.0), (250, 400), frame),
            (350, 300)
        );
        // bigger than the window: pinned to the top-left corner
        assert_eq!(
            render::menu_origin((10.0, 10.0), (1200, 900), frame),
            (0, 0)
        );
    }

    #[test]
    fn menu_rows_follow_the_tab() {
        let labels = |rows: Vec<(PaletteRow, String)>| -> Vec<String> {
            rows.into_iter().map(|(_, l)| l).collect()
        };
        let one = labels(context_menu_rows(false, 1, false, false));
        assert_eq!(
            keybinds::Action::Copy.label(),
            "نسخ التحديد",
            "the menu and the keymap share one label"
        );
        assert_eq!(one[0], "لصق", "paste first when nothing is selected");
        assert!(
            !one.iter().any(|l| l == keybinds::Action::Copy.label()),
            "copy stays off the menu until something is selected"
        );
        assert!(
            !one.iter().any(|l| l == keybinds::Action::CopyClean.label()),
            "clean copy stays off the menu until something is selected"
        );
        assert!(
            !one.iter().any(|l| l.contains("تكبير")),
            "no zoom with one pane"
        );
        assert!(one.iter().any(|l| l == "الإعدادات"));
        let sel = labels(context_menu_rows(true, 1, false, false));
        assert_eq!(
            &sel[..3],
            [
                keybinds::Action::Copy.label(),
                keybinds::Action::CopyClean.label(),
                keybinds::Action::Paste.label(),
            ]
        );
        let split = labels(context_menu_rows(false, 2, true, true));
        assert!(split.iter().any(|l| l == "إلغاء تكبير اللوحة"));
        assert!(split.iter().any(|l| l == "إيقاف الكتابة في كل اللوحات"));
        // every row is a keymap action, so the menu cannot drift from the
        // keys — except the one row that opens the shell menu, a list of
        // shells rather than an action
        let rows = context_menu_rows(true, 3, false, false);
        assert!(rows
            .iter()
            .all(|(r, _)| matches!(r, PaletteRow::Builtin(_) | PaletteRow::OpenShellMenu)));
        assert_eq!(
            rows.iter()
                .filter(|(r, _)| *r == PaletteRow::OpenShellMenu)
                .count(),
            1
        );
    }

    #[test]
    fn the_shell_menu_lists_every_detected_shell_and_marks_the_default() {
        let detected = vec![
            shells::Shell {
                name: "PowerShell".into(),
                command: "powershell.exe".into(),
            },
            shells::Shell {
                name: "Command Prompt".into(),
                command: "cmd.exe".into(),
            },
            shells::Shell {
                name: "Git Bash".into(),
                command: r"C:\PROGRA~1\Git\bin\bash.exe --login -i".into(),
            },
            shells::Shell {
                name: "WSL · Ubuntu".into(),
                command: "wsl.exe -d Ubuntu".into(),
            },
        ];
        let rows = shell_menu_rows(&detected, "cmd.exe");
        assert_eq!(rows.len(), 4);
        assert!(rows[2].0 == PaletteRow::Shell(detected[2].command.clone()));
        assert_eq!(rows[1].1, "Command Prompt · الافتراضية");
        assert_eq!(rows[3].1, "WSL · Ubuntu");
        // detection has not reported yet: PowerShell and cmd stand in
        let early = shell_menu_rows(&[], "powershell.exe");
        assert_eq!(early.len(), 2);
        assert!(early[1].0 == PaletteRow::Shell("cmd.exe".into()));
        assert_eq!(early[0].1, "PowerShell · الافتراضية");
    }

    #[test]
    fn a_saved_name_survives_and_old_files_still_parse() {
        let s = SavedState {
            tabs: vec![SavedTab {
                cwds: vec![None],
                weights: vec![1.0],
                orientation: 0,
                focused: 0,
                layout: None,
                name: Some("سجلات".into()),
                group: 0,
                shells: Vec::new(),
            }],
            active: 0,
            groups: Vec::new(),
            window: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(
            parse_state(&json).unwrap().tabs[0].name.as_deref(),
            Some("سجلات")
        );
        let old = r#"{"tabs":[{"cwds":[null]}],"active":0}"#;
        assert_eq!(parse_state(old).unwrap().tabs[0].name, None);
    }

    #[test]
    fn an_elevated_rename_or_group_change_does_not_write_session_json() {
        let dir = std::env::temp_dir().join(format!("bayan-session-elev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.json");
        std::fs::write(&path, b"ORIGINAL").unwrap();
        super::SESSION_FILE.with(|slot| *slot.borrow_mut() = Some(path.clone()));
        let mut app = super::session_app(super::test_proxy(), true);
        app.rename_tab(Some("مسؤول".into()));
        app.rename_group(Some("مجموعة".into()));
        app.move_tab_to_group(None);
        app.blocked_link = Some(BlockedLink {
            reason: "load".into(),
            shown: "config.json is unreadable".into(),
            confirm: None,
            is_app_error: true,
        });
        app.apply_admin_notice();
        let both = app
            .blocked_link
            .as_ref()
            .expect("config error and admin notice");
        assert!(both.shown.contains("config.json"));
        assert!(both.shown.contains("الإضافات"));
        app.open_posh();
        SESSION_FILE.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(std::fs::read(&path).unwrap(), b"ORIGINAL");
        assert!(app.posh.is_none());
        let card = app.blocked_link.as_ref().expect("the picker refusal");
        assert!(card.shown.contains("منتقي"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
fn session_app(proxy: EventLoopProxy<UserEvent>, elevated: bool) -> App {
    let mut app = App::new(proxy, config::UserConfig::default(), None, None);
    app.shell_profiles_ready = true;
    app.startup_pending = false;
    app.tabs = vec![Tab {
        panes: Vec::new(),
        layout: layout::Node::single(),
        focused: 0,
        title: "بيان".into(),
        name: None,
        attention: false,
        broadcast: false,
        zoomed: false,
        group: 0,
    }];
    app.next_id = 1;
    app.size = TermSize { cols: 80, rows: 24 };
    app.elevated = elevated;
    app
}

/// Smooth-scroll resets and pointer hits, through the `App` methods that
/// own them. Deleting the call inside `wakeup`, `relayout_active`,
/// `rebuild_renderer`, `locate`, or `try_report_press` fails one of these.
#[cfg(test)]
mod scroll_app_tests {
    use super::*;
    use std::time::Duration;

    fn bare() -> App {
        let mut app = App::new(test_proxy(), config::UserConfig::default(), None, None);
        app.startup_pending = false;
        app.next_id = 1;
        app.size = TermSize { cols: 40, rows: 12 };
        app
    }

    fn host(draw: bool) -> (App, u64, term::HeadlessWrite) {
        let mut app = bare();
        if draw {
            app.renderer = Some(render::Renderer::new(1.0, &app.config, 0.0));
        }
        let (cw, ch) = app
            .renderer
            .as_ref()
            .map(|r| (r.cell_w, r.cell_h))
            .unwrap_or((8.0, 16.0));
        let cols = 40usize;
        let rows = 12usize;
        app.test_content = Some((
            0,
            0,
            (cw * cols as f32).ceil() as i32,
            (ch * rows as f32).ceil() as i32,
        ));
        let (session, written) =
            term::Session::headless(TermSize { cols, rows }, app.proxy.clone(), 0);
        app.tabs.push(Tab {
            panes: vec![Pane {
                id: 0,
                session,
                command: String::new(),
                last_output: None,
                progress: None,
                mux: None,
            }],
            layout: layout::Node::single(),
            focused: 0,
            title: "t".into(),
            name: None,
            attention: false,
            broadcast: false,
            zoomed: false,
            group: 0,
        });
        (app, 0, written)
    }

    fn feed(app: &App, bytes: &[u8]) {
        let mut t = app.tabs[0].panes[0].session.term.lock();
        let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
        parser.advance(&mut *t, bytes);
    }

    fn display_offset(app: &App) -> usize {
        app.tabs[0].panes[0]
            .session
            .term
            .lock()
            .grid()
            .display_offset()
    }

    fn stale_fraction(app: &mut App, id: u64) {
        let mut st = scroll::PaneScroll::with_subline(0.4);
        let then = Instant::now()
            .checked_sub(Duration::from_millis(1_000))
            .unwrap_or_else(Instant::now);
        st.note_motion(then);
        app.smooth.insert(id, st);
    }

    fn fill_history(app: &App) {
        feed(app, "x\r\n".repeat(40).as_bytes());
    }

    #[test]
    fn new_output_at_the_bottom_clears_the_offset() {
        let (mut app, id, _) = host(false);
        stale_fraction(&mut app, id);
        app.wakeup(id);
        assert!(
            app.smooth.is_empty(),
            "a stale fraction at the bottom must go on output"
        );
    }

    #[test]
    fn new_output_while_scrolled_up_keeps_the_offset() {
        let (mut app, id, _) = host(false);
        fill_history(&app);
        app.tabs[0].panes[0]
            .session
            .term
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(3));
        assert!(display_offset(&app) > 0);
        stale_fraction(&mut app, id);
        app.wakeup(id);
        let sub = app.smooth.get(&id).map(|s| s.subline).unwrap_or(0.0);
        assert!((sub - 0.4).abs() < 0.001, "scrolled-up offset was {sub}");
    }

    #[test]
    fn entering_the_alt_screen_clears_a_resting_offset() {
        let (mut app, id, _) = host(false);
        // A live notch would survive output. Only the alt transition drops it.
        app.smooth_scroll_pane(id, 3.0, false);
        app.wakeup(id);
        assert!(app.smooth.contains_key(&id), "output ate a live notch");
        feed(&app, b"\x1b[?1049h");
        app.wakeup(id);
        assert!(
            app.smooth.is_empty(),
            "entering the alt screen left the offset"
        );
    }

    #[test]
    fn leaving_the_alt_screen_clears_a_resting_offset() {
        let (mut app, id, _) = host(false);
        feed(&app, b"\x1b[?1049h");
        app.wakeup(id);
        app.smooth_scroll_pane(id, 3.0, false);
        app.wakeup(id);
        assert!(app.smooth.contains_key(&id));
        feed(&app, b"\x1b[?1049l");
        app.wakeup(id);
        assert!(
            app.smooth.is_empty(),
            "leaving the alt screen left the offset"
        );
    }

    #[test]
    fn resize_clears_the_offset() {
        let mut app = bare();
        stale_fraction(&mut app, 1);
        stale_fraction(&mut app, 2);
        app.relayout_active();
        assert!(app.smooth.is_empty());
    }

    #[test]
    fn font_change_clears_the_offset() {
        let mut app = bare();
        stale_fraction(&mut app, 1);
        app.rebuild_renderer();
        assert!(app.smooth.is_empty());
    }

    /// Wheel-up while output is streaming. On b3746d5 every wakeup at the
    /// bottom called `drop_smooth`, so the notch never became a line.
    #[test]
    fn wheel_up_followed_by_output_wakeups_still_scrolls() {
        let (mut app, id, _) = host(false);
        fill_history(&app);
        app.smooth_scroll_pane(id, 3.0, false);
        // The loop finishes in microseconds, so a fresh `last_motion` would
        // keep the notch even if `scrolling` ignored `pending`. Age the
        // touch: only the pending notch may protect it from the wakeups.
        let aged = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        app.smooth.get_mut(&id).unwrap().note_motion(aged);
        for _ in 0..40 {
            app.smooth_at = Some(Instant::now() - Duration::from_millis(16));
            app.tick_smooth_scroll();
            app.wakeup(id);
        }
        let offset = display_offset(&app);
        assert!(
            offset >= 1,
            "output wiped the wheel; display offset stayed {offset}"
        );
    }

    #[test]
    fn a_slow_touchpad_drag_survives_output_at_the_bottom() {
        let (mut app, id, _) = host(false);
        fill_history(&app);
        for _ in 0..20 {
            app.smooth_scroll_pane(id, 0.1, true);
            app.wakeup(id);
        }
        let offset = display_offset(&app);
        assert!(
            offset >= 1,
            "slow touch motion was cleared before a line, offset {offset}"
        );
    }

    #[test]
    fn a_partial_offset_reports_the_shifted_row() {
        let (mut app, id, written) = host(true);
        feed(&app, b"\x1b[?1000h\x1b[?1006h");
        let mode = app.mode_of(id);
        assert!(mode.contains(TermMode::MOUSE_REPORT_CLICK));
        assert!(mode.contains(TermMode::SGR_MOUSE));
        let (cw, ch) = {
            let r = app.renderer.as_ref().unwrap();
            (r.cell_w, r.cell_h)
        };
        app.smooth.insert(id, scroll::PaneScroll::with_subline(0.4));
        let shift = scroll::offset_px(0.4, ch);
        assert!(shift >= 2, "cell {ch} shifted only {shift}px");
        let x = cw as f64 * 2.0 + 1.0;
        let y = ch as f64 + 1.0;
        assert_eq!((x / cw as f64).floor() as usize, 2);
        assert_eq!((y / ch as f64).floor() as usize, 1);
        app.cursor_pos = PhysicalPosition::new(x, y);
        let hit = app.locate(app.cursor_pos).unwrap();
        assert_eq!(hit.2, 0, "locate ignored the offset");
        assert!(app.try_report_press(mouse::Btn::Left));
        assert_eq!(written.wait(), b"\x1b[<0;3;1M");

        app.smooth.remove(&id);
        app.cursor_pos = PhysicalPosition::new(x, y);
        let hit = app.locate(app.cursor_pos).unwrap();
        assert_eq!(hit.2, 1);
        assert!(app.try_report_press(mouse::Btn::Left));
        assert_eq!(written.wait(), b"\x1b[<0;3;2M");
    }

    #[test]
    fn the_peek_sliver_is_not_reported_as_the_top_row() {
        let (mut app, id, _) = host(true);
        feed(&app, b"\x1b[?1000h\x1b[?1006h");
        let ch = app.renderer.as_ref().unwrap().cell_h;
        app.smooth.insert(id, scroll::PaneScroll::with_subline(0.4));
        let shift = scroll::offset_px(0.4, ch) as f64;
        let y = 1.0;
        assert!(shift > y, "no peek sliver: shift {shift}");
        assert_eq!((y / ch as f64).floor() as usize, 0);
        app.cursor_pos = PhysicalPosition::new(4.0, y);
        let hit = app.locate(app.cursor_pos).unwrap();
        assert_eq!(hit.2, -1);
        assert!(!app.try_report_press(mouse::Btn::Left));
    }

    #[test]
    fn history_row_click_keeps_the_grid_column_when_reordering_is_off() {
        let (mut app, id, _) = host(true);
        let ch = app.renderer.as_ref().unwrap().cell_h;
        app.smooth.insert(id, scroll::PaneScroll::with_subline(0.5));
        let shift = scroll::offset_px(0.5, ch);
        assert!(shift >= 4, "cell {ch} shift {shift}");
        let rect = app.pane_rects()[0];
        app.renderer
            .as_mut()
            .unwrap()
            .plant_hit(rect, scroll::PEEK_ROW, 0, rect.2.max(1), 7);
        {
            let mut meta = app.tabs[0].panes[0].session.meta.lock_or_recover();
            meta.running_cmd = "demo".into();
        }
        app.cursor_pos = PhysicalPosition::new(2.0, 1.0);
        let on = app.locate(app.cursor_pos).unwrap();
        assert_eq!(on.2, -1);
        assert_eq!(on.3, 7, "implicit mode uses the shaped column");
        {
            let mut meta = app.tabs[0].panes[0].session.meta.lock_or_recover();
            meta.bidi_mut().mode = bidi_ctl::BidiMode::Explicit;
        }
        let off = app.locate(app.cursor_pos).unwrap();
        assert_eq!(off.2, -1);
        assert_eq!(off.3, 0, "explicit BDSM stays on the grid column");
    }
}
