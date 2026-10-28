//! Bayan (بيان) — an Arabic-first, agent-ready terminal.
//!
//! Milestone M1: one window, one PowerShell over ConPTY, correct Arabic
//! shaping and BiDi from day one. Architecture mirrors EasyTer (Python/Qt),
//! whose regression suites serve as the behavioral specification.

// release builds are pure GUI apps: no companion console window
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bidi;
mod bidi_ctl;
mod chrome;
mod config;
mod gpu;
mod input;
mod keybinds;
mod keyroute;
mod keys;
mod kitty;
mod link;
mod mouse;
mod mru;
mod osc7;
mod plugins;
mod posh;
mod render;
mod shells;
mod term;
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
const QUAKE_ID: usize = 0xB1AA;

/// The letter a Ctrl+<letter> event represents, resolved the way real
/// terminals do it: by the PHYSICAL key position first. This is what makes
/// Ctrl+C/Ctrl+F work on an Arabic keyboard layout, where the F key's
/// logical character is "ب" — and it also covers the Windows quirk of Ctrl
/// composing C0 control chars into the logical key (Ctrl+F = '\u{6}').
fn key_letter(event: &KeyEvent) -> Option<char> {
    use winit::keyboard::{KeyCode, PhysicalKey};
    if let PhysicalKey::Code(c) = event.physical_key {
        use KeyCode::*;
        let l = match c {
            KeyA => 'a',
            KeyB => 'b',
            KeyC => 'c',
            KeyD => 'd',
            KeyE => 'e',
            KeyF => 'f',
            KeyG => 'g',
            KeyH => 'h',
            KeyI => 'i',
            KeyJ => 'j',
            KeyK => 'k',
            KeyL => 'l',
            KeyM => 'm',
            KeyN => 'n',
            KeyO => 'o',
            KeyP => 'p',
            KeyQ => 'q',
            KeyR => 'r',
            KeyS => 's',
            KeyT => 't',
            KeyU => 'u',
            KeyV => 'v',
            KeyW => 'w',
            KeyX => 'x',
            KeyY => 'y',
            KeyZ => 'z',
            _ => return ctrl_letter(&event.logical_key),
        };
        return Some(l);
    }
    ctrl_letter(&event.logical_key)
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
fn active_after_remove(active: usize, removed: usize, len_after: usize) -> usize {
    if len_after == 0 {
        return 0;
    }
    let a = if removed < active { active - 1 } else { active };
    a.min(len_after - 1)
}

/// Bytes a wheel notch sends to a full-screen program (EasyTer's rule): a
/// mouse report only when the app enabled mouse tracking — SGR encoded if it
/// asked for that (DECSET 1006), X10 bytes otherwise — else an arrow key that
/// honours DECCKM. `n` > 0 is a notch up. Bytes, not a String: X10 packs
/// each value into ONE byte (32 + v), and past column 95 that byte is >= 0x80,
/// which a String would UTF-8-encode into two.
#[cfg(test)]
fn wheel_seq(mode: TermMode, n: i32, row: usize, col: usize) -> Vec<u8> {
    let btn = if n > 0 { 64 } else { 65 };
    if mode.intersects(TermMode::MOUSE_MODE) {
        if mode.contains(TermMode::SGR_MOUSE) {
            format!("\x1b[<{btn};{col};{row}M").into_bytes()
        } else {
            let b = |v: usize| (32 + v.min(223)) as u8;
            vec![0x1b, b'[', b'M', b(btn), b(col), b(row)]
        }
    } else {
        let ch = if n > 0 { b'A' } else { b'B' };
        if mode.contains(TermMode::APP_CURSOR) {
            vec![0x1b, b'O', ch]
        } else {
            vec![0x1b, b'[', ch]
        }
    }
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
        if std::env::var_os("BAYAN_PROFILE").is_some() {
            let _ = std::fs::write(std::env::temp_dir().join("bayan_profile.log"), "");
        }
    }

    pub fn mark(label: &str) {
        if std::env::var_os("BAYAN_PROFILE").is_none() {
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

use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::TermMode;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use term::{Session, TermSize};

pub enum UserEvent {
    ShellProfiles(Vec<shells::Shell>),
    /// New PTY output was parsed in tab `id`: repaint (and busy-dot it).
    Wakeup(u64),
    /// Tab `id`'s emulator answers its child (DSR/DA replies TUIs block on).
    PtyWrite(u64, String),
    /// The font system finished loading on its background thread.
    RendererReady(Box<render::Renderer>),
    /// Pane `id` asked to set the clipboard via OSC 52. Applied only when
    /// `allow_osc52_write` says so — the default is to drop it, and a write
    /// from any pane but the focused one of the active tab is dropped too.
    ClipboardSet(u64, String),
    /// Pane `id`'s shell reported its working directory (OSC 7 or OSC 9;9).
    Cwd(u64, String),
    /// Pane `id` rang the bell (Claude waiting for an approval).
    Bell(u64),
    /// A command that ran ≥6s in pane `id` just finished (ok?).
    CommandFinished(u64, bool),
    /// Pane `id`'s program asked for a desktop notification (title, body).
    Notify(u64, String, String),
    /// Pane `id` reported OSC 9;4 progress (None clears it).
    Progress(u64, Option<term::Progress>),
    /// Pane `id`'s shell exited.
    Exit(u64),
    /// A link open failed, or `settle` refused it, off the UI thread.
    /// `reason` is already [`link::RefuseReason::label`].
    LinkFailed {
        reason: String,
        shown: String,
    },
}

/// The link under the pointer. Stable while the pointer stays inside it,
/// so a hover does not rebuild the frame on every pixel.
#[derive(PartialEq, Eq)]
struct LinkHover {
    pane_id: u64,
    row: usize,
    start_col: usize,
    end_col: usize,
    target: link::StatusParts,
}

/// A Ctrl+click that was refused, or a userinfo URL waiting for Enter.
/// Esc dismisses either. Enter opens only when `confirm_uri` is set;
/// a refusal leaves it empty so Enter cannot launch the target.
struct BlockedLink {
    reason: String,
    shown: String,
    confirm_uri: Option<String>,
}

/// Text held for the confirm card: the bytes to send, and why the card is up.
struct PendingPaste {
    text: String,
    lines: usize,
    chars: usize,
    reason: String,
}

/// One terminal pane (a tab holds one or more).
struct Pane {
    id: u64,
    session: Session,
    /// when this pane last produced output — feeds the busy dot
    last_output: Option<Instant>,
    /// the program's OSC 9;4 progress report, until it clears it
    progress: Option<term::Progress>,
}

#[derive(Clone, Copy, PartialEq)]
enum Orientation {
    Row,    // side by side (Ctrl+Shift+E)
    Column, // stacked (Ctrl+Shift+O)
}

/// One tab: a set of panes split along one axis (v1 of EasyTer's tree).
struct Tab {
    panes: Vec<Pane>,
    /// relative pane sizes along the axis (dragging a divider edits these)
    weights: Vec<f32>,
    focused: usize,
    orientation: Orientation,
    /// cwd basename of the focused pane (falls back to a plain name)
    title: String,
    /// a background pane rang the bell: show the amber dot until visited
    attention: bool,
}

impl Tab {
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

/// Weighted split of a content rect along one axis, 1px gaps between panes.
fn split_rects(
    content: render::Rect,
    weights: &[f32],
    orientation: Orientation,
) -> Vec<render::Rect> {
    let (x, y, w, h) = content;
    let n = weights.len().max(1) as i32;
    let gap = 1;
    let total: f32 = weights.iter().sum::<f32>().max(f32::EPSILON);
    let avail = match orientation {
        Orientation::Row => w - gap * (n - 1),
        Orientation::Column => h - gap * (n - 1),
    } as f32;
    let mut out = Vec::with_capacity(weights.len());
    let mut cursor = match orientation {
        Orientation::Row => x,
        Orientation::Column => y,
    };
    for (i, wt) in weights.iter().enumerate() {
        let len = if i as i32 == n - 1 {
            // the last pane absorbs all rounding remainders
            match orientation {
                Orientation::Row => x + w - cursor,
                Orientation::Column => y + h - cursor,
            }
        } else {
            (avail * wt / total) as i32
        };
        out.push(match orientation {
            Orientation::Row => (cursor, y, len, h),
            Orientation::Column => (x, cursor, w, len),
        });
        cursor += len + gap;
    }
    out
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
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedState {
    tabs: Vec<SavedTab>,
    active: usize,
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
            })
            .collect(),
        active: legacy.active,
        window: None,
    })
}

fn state_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE").ok()?;
    Some(
        std::path::Path::new(&home)
            .join(".bayan")
            .join("session.json"),
    )
}

/// Multi-click detection: 1 = simple, 2 = word (semantic), 3 = line.
struct ClickTracker {
    last: Option<(Instant, (usize, usize))>,
    count: u8,
}

impl ClickTracker {
    fn new() -> Self {
        Self {
            last: None,
            count: 0,
        }
    }

    fn click(&mut self, now: Instant, cell: (usize, usize)) -> u8 {
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
                format!("تبويب جديد — {}", shell.name),
            )
        })
        .collect();
    rows.push((
        PaletteRow::InsertCommand("claude"),
        "إدراج أمر كلاود — claude".into(),
    ));
    rows.push((
        PaletteRow::InsertCommand("claude --continue"),
        "متابعة محادثة كلاود — claude --continue".into(),
    ));
    let mut seen = std::collections::HashSet::new();
    for span in links {
        let text = match &span.kind {
            link::LinkKind::Osc { uri } | link::LinkKind::Url { uri } => uri,
            link::LinkKind::File { path, .. } => path,
        };
        if text.is_empty() || !seen.insert(text.clone()) {
            continue;
        }
        let shown = link::fit_status_bar(&span.tooltip(None), 90).text();
        rows.push((PaletteRow::CopyText(text.clone()), format!("نسخ — {shown}")));
        if seen.len() >= 100 {
            break;
        }
    }
    rows
}

/// Ctrl+Shift+P: type to filter, arrows to move, Enter to run.
struct Palette {
    query: String,
    sel: usize,
    extras: Vec<(PaletteRow, String)>,
    pressed: Option<(usize, PaletteRow)>,
    wheel: f32,
}

/// A frame's snapshot of the picker: (names, previews, selected row).
type PoshView = (Vec<String>, Vec<Option<Vec<u8>>>, usize);

/// The prompt picker: theme names with LIVE previews rendered by oh-my-posh
/// itself, so what you see is the prompt you get.
struct PoshPicker {
    names: Vec<String>,
    /// preview ANSI per name, fetched once on open (a subprocess per row)
    previews: Vec<Option<Vec<u8>>>,
    sel: usize,
}

struct App {
    /// the prompt picker, when open
    posh: Option<PoshPicker>,
    /// when a PTY-triggered toast last fired — rate-limits program output
    /// that asks for attention in a loop
    last_toast: Option<Instant>,
    /// most-recently-used tab history for Ctrl+Tab
    mru: mru::Mru,
    /// the command palette, when open
    palette: Option<Palette>,
    shell_profiles: Vec<shells::Shell>,
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
    cursor_pos: PhysicalPosition<f64>,
    /// Grab, motion dedupe, focus-in memory, and the local selection drag.
    /// A focus loss drops all of them together.
    mouse: mouse::Route,
    /// the pane a mouse selection started in (drags don't cross panes)
    sel_pane: usize,
    /// divider being dragged (between pane i and i+1)
    drag_divider: Option<usize>,
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
    search: Option<SearchState>,
    clipboard: Option<arboard::Clipboard>,
    /// Claude mode follows (alt screen + claude command) automatically;
    /// F2 switches to manual and back (EasyTer's toggle semantics).
    auto_follow: bool,
    claude_manual: bool,
    config: config::UserConfig,
    /// every rebindable action's effective chord (defaults + config)
    keymap: Vec<(keybinds::Action, keybinds::Chord)>,
    /// the shortcuts editor overlay, when open
    shortcuts: Option<ShortcutsState>,
    /// last seen atlas generation (a reset schedules a healing redraw)
    atlas_generation: u32,
    /// Ctrl+wheel zoom, in points on top of the configured size.
    font_delta: f32,
    renderer_building: bool,
    inflight_delta: f32,
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
}

impl App {
    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// (Re)build the renderer off-thread for the current scale, config and
    /// zoom. Debounced: one build in flight; a stale result triggers another.
    fn rebuild_renderer(&mut self) {
        if self.renderer_building {
            return;
        }
        let Some(w) = &self.window else { return };
        self.renderer_building = true;
        self.inflight_delta = self.font_delta;
        let proxy = self.proxy.clone();
        let scale = w.scale_factor() as f32;
        let cfg = self.config.clone();
        let delta = self.font_delta;
        std::thread::spawn(move || {
            let r = render::Renderer::new(scale, &cfg, delta);
            let _ = proxy.send_event(UserEvent::RendererReady(Box::new(r)));
        });
    }

    // ---- settings panel ----

    /// Index of the active theme in the gallery order (built-ins, then
    /// plugin themes) — the index the panel highlights and apply_theme takes.
    fn active_theme(&self) -> usize {
        let name = self
            .config
            .theme
            .as_deref()
            .unwrap_or(render::DEFAULT_THEME);
        if let Some(i) = render::THEMES.iter().position(|t| t.name == name) {
            return i;
        }
        plugins::registry()
            .themes
            .iter()
            .position(|t| t.name == name)
            .map_or(0, |i| render::THEMES.len() + i)
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
            self.config.theme = Some(name);
            self.config.bg = Some(hex(t.bg));
            self.config.fg = Some(hex(t.fg));
            self.config.palette = Some(t.palette.iter().map(|&c| hex(c)).collect());
            self.rebuild_renderer();
            self.request_redraw();
        }
    }

    fn change_font_size(&mut self, delta: i32) {
        let s = (self.config.font_size.unwrap_or(render::DEFAULT_FONT_SIZE) + delta as f32)
            .clamp(8.0, 40.0);
        self.config.font_size = Some(s);
        self.rebuild_renderer();
        self.request_redraw();
    }

    fn toggle_ligatures(&mut self) {
        let v = !self.config.ligatures.unwrap_or(true);
        self.config.ligatures = Some(v);
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
        let i = choices.iter().position(|c| c == cur).unwrap_or(0) as i32;
        let n = choices.len() as i32;
        let next = choices[(((i + dir) % n + n) % n) as usize].clone();
        self.config.font_family = Some(next);
        self.rebuild_renderer();
        self.request_redraw();
    }

    fn set_cursor_style(&mut self, s: config::CursorStyle) {
        self.config.cursor_style = Some(s.as_str().to_string());
        self.request_redraw(); // read per-frame; no rebuild needed
    }

    fn toggle_blink(&mut self) {
        self.config.cursor_blink = Some(!self.config.cursor_blink_on());
        self.last_input = Instant::now(); // preview the blink right away
        self.request_redraw();
    }

    /// Walk the scrollback presets. Applies to new tabs only (the universal
    /// terminal convention — live sessions keep their buffer).
    fn change_scrollback(&mut self, dir: i32) {
        let cur = self.config.scrollback_lines();
        let steps = config::SCROLLBACK_STEPS;
        // nearest preset, then step
        let i = steps
            .iter()
            .position(|&s| s >= cur)
            .unwrap_or(steps.len() - 1) as i32;
        let j = (i + dir).clamp(0, steps.len() as i32 - 1) as usize;
        self.config.scrollback = Some(steps[j] as u32);
        self.request_redraw();
    }

    fn toggle_copy_on_select(&mut self) {
        self.config.copy_on_select = Some(!self.config.copy_on_select_on());
        self.request_redraw();
    }

    fn set_bell(&mut self, mode: config::BellMode) {
        self.config.bell = Some(mode.as_str().to_string());
        if mode == config::BellMode::Sound {
            beep(); // preview the mode you just picked
        }
        self.request_redraw();
    }

    /// Step to the previous/next installed shell. New tabs only.
    fn cycle_shell(&mut self, dir: i32) {
        let choices = term::shell_choices();
        if choices.len() < 2 {
            return;
        }
        let cur = self.config.shell_program();
        let i = choices.iter().position(|c| *c == cur).unwrap_or(0) as i32;
        let n = choices.len() as i32;
        self.config.shell = Some(choices[(((i + dir) % n + n) % n) as usize].clone());
        self.request_redraw();
    }

    fn change_padding(&mut self, dir: i32) {
        let cur = self.config.padding_px() as u32;
        let steps = config::PADDING_STEPS;
        let i = steps
            .iter()
            .position(|&s| s >= cur)
            .unwrap_or(steps.len() - 1) as i32;
        let j = (i + dir).clamp(0, steps.len() as i32 - 1) as usize;
        self.config.padding = Some(steps[j]);
        self.relayout_active(); // pane rects (and PTY sizes) follow the inset
        self.request_redraw();
    }

    fn change_opacity(&mut self, dir: i32) {
        let cur = (self.config.opacity_level() * 100.0).round() as u32;
        let steps = config::OPACITY_STEPS;
        let i = steps
            .iter()
            .position(|&s| s >= cur)
            .unwrap_or(steps.len() - 1) as i32;
        let j = (i + dir).clamp(0, steps.len() as i32 - 1) as usize;
        self.config.opacity = Some(steps[j] as f32 / 100.0);
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
        self.save_state(); // tabs + cwds greet you tomorrow
        el.exit();
    }

    fn resolve_close(&mut self, confirm: bool, el: &ActiveEventLoop) {
        if let Some(target) = self.pending_close.take().filter(|_| confirm) {
            match target {
                CloseTarget::Pane(ti, pi) => self.remove_pane(ti, pi, el),
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
        let Some(hwnd) = self.hwnd() else { return };
        use windows_sys::Win32::Graphics::Dwm::{
            DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
            DWMWCP_ROUND,
        };
        let dark = 1i32;
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
        self.config.hide_single_tab = Some(!self.config.hide_single_tab_on());
        self.relayout_active(); // the content area gains/loses the bar strip
        self.request_redraw();
    }

    fn toggle_confirm_close(&mut self) {
        self.config.confirm_close = Some(!self.config.confirm_close_on());
        self.request_redraw();
    }

    fn toggle_notifications(&mut self) {
        let on = !self.config.notifications_on();
        self.config.notifications = Some(on);
        if on {
            // preview the toast you just turned on
            if let Some(h) = self.hwnd() {
                toast::show(h, "الإشعارات مفعّلة", "هكذا يبدو إشعار اكتمال الأمر");
            }
        }
        self.request_redraw();
    }

    /// Open the prompt picker, rendering every theme once up front. One
    /// subprocess per theme is fine at ~11 themes and keeps scrolling instant;
    /// rendering lazily would stutter exactly while the user is browsing.
    fn open_posh(&mut self) {
        let names = posh::available();
        if names.is_empty() {
            return; // no oh-my-posh themes: nothing to pick from
        }
        let previews = names.iter().map(|n| posh::preview(n)).collect();
        let cur = posh::current();
        let sel = cur
            .and_then(|c| names.iter().position(|n| *n == c))
            .unwrap_or(0);
        self.posh = Some(PoshPicker {
            names,
            previews,
            sel,
        });
        self.request_redraw();
    }

    fn posh_input(&mut self, event: &KeyEvent) {
        use winit::keyboard::{Key, NamedKey};
        let Some(p) = self.posh.as_mut() else { return };
        let n = p.names.len();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.posh = None,
            Key::Named(NamedKey::ArrowDown) => p.sel = (p.sel + 1) % n,
            Key::Named(NamedKey::ArrowUp) => p.sel = (p.sel + n - 1) % n,
            Key::Named(NamedKey::Enter) => {
                let name = p.names[p.sel].clone();
                self.posh = None;
                match posh::set_theme(&name) {
                    Ok(()) => eprintln!("bayan: prompt theme set to {name} (new tabs)"),
                    Err(e) => eprintln!("bayan: prompt theme failed: {e}"),
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
        let extras = palette_extras(&self.shell_profiles, &links);
        self.palette = Some(Palette {
            query,
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
        let Some(p) = self.palette.as_mut() else {
            return;
        };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.palette = None,
            Key::Named(NamedKey::Backspace) => {
                p.query.pop();
                p.sel = 0;
            }
            Key::Named(NamedKey::ArrowDown) => {
                let n = self
                    .palette_rows(&self.palette.as_ref().unwrap().query.clone())
                    .len();
                if n > 0 {
                    let p = self.palette.as_mut().unwrap();
                    p.sel = (p.sel + 1) % n;
                }
            }
            Key::Named(NamedKey::ArrowUp) => {
                let n = self
                    .palette_rows(&self.palette.as_ref().unwrap().query.clone())
                    .len();
                if n > 0 {
                    let p = self.palette.as_mut().unwrap();
                    p.sel = (p.sel + n - 1) % n;
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
                p.query.push_str(c);
                p.sel = 0; // a new filter invalidates the old position
            }
            _ => {}
        }
        self.request_redraw();
    }

    /// Palette rows matching the query, built-ins first then plugin actions.
    /// Matching is case-insensitive and substring, on the label the user
    /// actually sees — searching a hidden id would be a guessing game.
    fn palette_rows(&self, query: &str) -> Vec<(PaletteRow, String)> {
        let q = query.trim().to_lowercase();
        let hit = |label: &str| q.is_empty() || label.to_lowercase().contains(&q);
        let mut rows: Vec<(PaletteRow, String)> = Vec::new();
        for a in keybinds::Action::ALL {
            if hit(a.label()) {
                rows.push((PaletteRow::Builtin(a), a.label().to_string()));
            }
        }
        for (i, act) in plugins::registry().actions.iter().enumerate() {
            if hit(&act.label) {
                rows.push((PaletteRow::Plugin(i), act.label.clone()));
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
        match row {
            // the SAME dispatch a keybinding uses, not a parallel copy
            PaletteRow::Builtin(a) => self.perform_action(a, el),
            PaletteRow::Shell(command) => {
                let cwd = self
                    .session()
                    .and_then(|s| s.meta.lock().unwrap().spawn_cwd());
                let before = self.tabs.len();
                self.spawn_tab_command(cwd, &command);
                if self.tabs.len() == before && self.blocked_link.is_none() {
                    self.blocked_link = Some(BlockedLink {
                        reason: "تعذر تشغيل الطرفية".into(),
                        shown: link::preview(&command, 240),
                        confirm_uri: None,
                    });
                }
                self.update_title();
            }
            PaletteRow::CopyText(text) => self.set_clipboard(text),
            PaletteRow::InsertCommand(text) => {
                self.pending_paste = Some(PendingPaste {
                    text: text.into(),
                    lines: 1,
                    chars: text.chars().count(),
                    reason: "إدراج أمر من لوحة الأوامر".into(),
                });
            }
            PaletteRow::Plugin(i) => {
                if let Some(act) = plugins::registry().actions.get(i) {
                    let text = act.send.clone();
                    if let Some(s) = self.session_mut() {
                        s.write(text.as_bytes());
                    }
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
                    .and_then(|s| s.meta.lock().unwrap().spawn_cwd());
                self.spawn_tab(cwd);
                self.update_title();
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
            ResetKeyboard => {
                if let Some(s) = self.session_mut() {
                    s.reset_keyboard();
                }
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
                st.flash = Some("الاختصار يحتاج Ctrl أو Alt أو مفتاح F".to_string());
                st.capturing = false;
                self.request_redraw();
                return;
            }
            let action = keybinds::Action::ALL[st.sel];
            if let Some(owner) = keybinds::lookup(&self.keymap, chord) {
                if owner != action {
                    st.flash = Some(format!("«{}» يستخدم هذا الاختصار", owner.label()));
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
                config::save(&self.config);
                self.request_redraw();
            }
            Key::Named(NamedKey::ArrowUp) => {
                st.sel = (st.sel + n - 1) % n;
                st.flash = None;
                self.request_redraw();
            }
            Key::Named(NamedKey::ArrowDown) => {
                st.sel = (st.sel + 1) % n;
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
        let kb = self.config.keybinds.get_or_insert_with(Default::default);
        if chord == action.default_chord() {
            kb.remove(action.id());
        } else {
            kb.insert(action.id().to_string(), chord.to_config());
        }
        if self.config.keybinds.as_ref().is_some_and(|m| m.is_empty()) {
            self.config.keybinds = None;
        }
        self.keymap = keybinds::effective_map(&self.config);
        self.request_redraw();
    }

    fn clear_binding(&mut self, action: keybinds::Action) {
        if let Some(kb) = self.config.keybinds.as_mut() {
            kb.remove(action.id());
            if kb.is_empty() {
                self.config.keybinds = None;
            }
        }
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
            config::save(&self.config);
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
        let cmd = pane.session.meta.lock().unwrap().running_cmd.clone();
        (!cmd.is_empty()).then_some(cmd)
    }

    /// How many panes across all tabs are mid-command right now.
    fn running_count(&self) -> usize {
        self.tabs
            .iter()
            .flat_map(|t| &t.panes)
            .filter(|p| !p.session.meta.lock().unwrap().running_cmd.is_empty())
            .count()
    }

    fn close_settings(&mut self) {
        self.settings = None;
        config::save(&self.config); // persist on close
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
        if render::rect_hit(lay.shortcuts_btn, px, py) {
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
                        let m = p.session.meta.lock().unwrap();
                        if !m.running_cmd.is_empty() {
                            format!("▶ {}", m.running_cmd)
                        } else if let Some(cwd) = &m.cwd {
                            format!("خامل · {cwd}")
                        } else {
                            "خامل".to_string()
                        }
                    })
                    .unwrap_or_default();
                let n = t.panes.len();
                let status = if n > 1 {
                    format!("{status}   ({n} لوحات)")
                } else {
                    status
                };
                render::CockpitEntry {
                    title: t.title.clone(),
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
                let m = s.meta.lock().unwrap();
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
        self.spawn_pane_command(cwd, &self.config.shell_program())
    }

    fn spawn_pane_command(&mut self, cwd: Option<&str>, command: &str) -> Option<Pane> {
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
        ) {
            Ok(session) => Some(Pane {
                id,
                session,
                last_output: None,
                progress: None,
            }),
            Err(e) => {
                eprintln!("bayan: spawn failed: {e}");
                None
            }
        }
    }

    fn spawn_tab(&mut self, cwd: Option<String>) {
        self.spawn_tab_command(cwd, &self.config.shell_program());
    }

    fn spawn_tab_command(&mut self, cwd: Option<String>, command: &str) {
        // Same gate as spawn: a restored UNC path must not become the tab
        // title either. accept_local_cwd does not touch the filesystem.
        let cwd = cwd.and_then(|c| term::accept_local_cwd(&c));
        let title = cwd
            .as_deref()
            .and_then(|c| c.rsplit(['\\', '/']).next().map(str::to_string))
            .unwrap_or_else(|| "بيان".to_string());
        if let Some(pane) = self.spawn_pane_command(cwd.as_deref(), command) {
            // this becomes the active tab without switch_tab, so a button
            // held on the previous tab has to go up first
            self.abandon_mouse();
            self.tabs.push(Tab {
                panes: vec![pane],
                weights: vec![1.0],
                focused: 0,
                orientation: Orientation::Row,
                title,
                attention: false,
            });
            self.active = self.tabs.len() - 1;
            self.mru.touch(self.active);
            self.relayout_active();
            self.sync_focus(None);
        }
    }

    /// Pane rects for the active tab within the window's content area.
    fn pane_rects(&self) -> Vec<render::Rect> {
        let (Some(r), Some(w), Some(t)) = (
            self.renderer.as_ref(),
            self.window.as_ref(),
            self.tabs.get(self.active),
        ) else {
            return Vec::new();
        };
        let px = w.inner_size();
        let bar = r.tab_bar_h().round() as i32;
        // the padding setting insets the whole content area evenly
        let pad = self.config.padding_px();
        let content = (
            pad,
            bar + pad,
            (px.width as i32 - pad * 2).max(1),
            (px.height as i32 - bar - pad * 2).max(1),
        );
        split_rects(content, &t.weights, t.orientation)
    }

    /// Drag divider `i` to the pointer: redistribute the PAIR's weight.
    fn drag_divider_to(&mut self, i: usize, pos: PhysicalPosition<f64>) {
        let rects = self.pane_rects();
        let Some(t) = self.tabs.get_mut(self.active) else {
            return;
        };
        if i + 1 >= rects.len() {
            return;
        }
        let (a, b) = (rects[i], rects[i + 1]);
        let (start, span, p) = match t.orientation {
            Orientation::Row => (a.0 as f64, (b.0 + b.2 - a.0) as f64, pos.x),
            Orientation::Column => (a.1 as f64, (b.1 + b.3 - a.1) as f64, pos.y),
        };
        if span <= 0.0 {
            return;
        }
        // keep both panes usable: at least ~80px each
        let min = (80.0 / span).min(0.45);
        let rel = ((p - start) / span).clamp(min, 1.0 - min) as f32;
        let pair = t.weights[i] + t.weights[i + 1];
        t.weights[i] = pair * rel;
        t.weights[i + 1] = pair - t.weights[i];
        self.relayout_active();
        self.request_redraw();
    }

    /// Resize every pane of the ACTIVE tab to its rect (background tabs
    /// re-layout when they become active — the standard terminal tradeoff).
    fn relayout_active(&mut self) {
        // Pane rows moved. Cached link spans would underline the wrong cells.
        self.link_cache.clear();
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
        if let Some(t) = self.tabs.get_mut(self.active) {
            for (pane, rect) in t.panes.iter_mut().zip(&rects) {
                let g = TermSize {
                    cols: ((rect.2 as f32 / cw) as usize).max(10),
                    rows: ((rect.3 as f32 / ch) as usize).max(3),
                };
                pane.session.resize(g);
            }
        }
    }

    /// Split the active tab (EasyTer: Ctrl+Shift+E side-by-side, O stacked).
    fn split(&mut self, orientation: Orientation) {
        // four panes per tab is the v1 cap (one axis, equal sizes)
        if self
            .tabs
            .get(self.active)
            .is_none_or(|t| t.panes.len() >= 4)
        {
            return;
        }
        let cwd = self
            .session()
            .and_then(|s| s.meta.lock().unwrap().spawn_cwd());
        let Some(pane) = self.spawn_pane(cwd.as_deref()) else {
            return;
        };
        if let Some(t) = self.tabs.get_mut(self.active) {
            if t.panes.len() == 1 {
                t.orientation = orientation;
            }
            t.panes.push(pane);
            t.weights.push(1.0);
            t.focused = t.panes.len() - 1;
        }
        self.relayout_active();
        self.sync_focus(None);
        self.request_redraw();
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
        if matches!(self.pending_close, Some(CloseTarget::Pane(..))) {
            self.pending_close = None;
        }
        let mut pane = t.panes.remove(pi);
        if pi < t.weights.len() {
            t.weights.remove(pi);
        }
        pane.session.kill();
        if t.panes.is_empty() {
            self.tabs.remove(ti);
            if self.tabs.is_empty() {
                self.quit(el);
                return;
            }
            self.mru.removed(ti);
            self.active = active_after_remove(self.active, ti, self.tabs.len());
            self.mru.touch(self.active);
        } else if t.focused >= t.panes.len() {
            t.focused = t.panes.len() - 1;
        }
        self.relayout_active();
        self.update_title();
        self.sync_focus(None);
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
        if let Some(t) = self.tabs.get_mut(idx) {
            t.attention = false; // you're looking at it now
        }
        self.search = None; // the bar belongs to the tab that opened it
        self.relayout_active(); // this tab's layout may differ
        self.update_title();
        self.sync_focus(None);
        self.request_redraw();
    }

    fn update_title(&self) {
        if let (Some(w), Some(t)) = (&self.window, self.tabs.get(self.active)) {
            // the window title stays just "Bayan"; the folder shows in the
            // tab (no app-name-looking cwd suffix in the title bar)
            let _ = t;
            w.set_title("Bayan — بيان");
        }
    }

    fn save_state(&self) {
        let Some(path) = state_path() else { return };
        let state = SavedState {
            tabs: self
                .tabs
                .iter()
                .map(|t| SavedTab {
                    cwds: t
                        .panes
                        .iter()
                        .map(|p| {
                            // don't persist a path we would refuse to start in
                            p.session
                                .meta
                                .lock()
                                .unwrap()
                                .cwd
                                .as_deref()
                                .and_then(term::accept_local_cwd)
                        })
                        .collect(),
                    weights: t.weights.clone(),
                    orientation: match t.orientation {
                        Orientation::Row => 0,
                        Orientation::Column => 1,
                    },
                    focused: t.focused,
                })
                .collect(),
            active: self.active,
            window: self.normal_geom.map(|g| WindowGeom {
                maximized: self.window.as_ref().is_some_and(|w| w.is_maximized()),
                ..g
            }),
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(path, json);
        }
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

    fn restore_state(&mut self) {
        if !self.config.restore_tabs.unwrap_or(false) {
            self.spawn_tab(None);
            return;
        }
        let saved = load_saved_state();
        match saved {
            Some(state) if !state.tabs.is_empty() => {
                for st in state.tabs {
                    let mut cwds = st.cwds.into_iter();
                    self.spawn_tab(cwds.next().flatten());
                    for cwd in cwds.take(3) {
                        // re-split with the saved axis; weights come after
                        if let Some(pane) = self.spawn_pane(cwd.as_deref()) {
                            if let Some(t) = self.tabs.last_mut() {
                                t.panes.push(pane);
                                t.weights.push(1.0);
                            }
                        }
                    }
                    if let Some(t) = self.tabs.last_mut() {
                        t.orientation = if st.orientation == 1 {
                            Orientation::Column
                        } else {
                            Orientation::Row
                        };
                        if st.weights.len() == t.panes.len() {
                            let sane = st.weights.iter().all(|w| w.is_finite() && *w > 0.0);
                            if sane {
                                t.weights = st.weights;
                            }
                        }
                        t.focused = st.focused.min(t.panes.len() - 1);
                    }
                }
                self.active = state.active.min(self.tabs.len().saturating_sub(1));
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
            mode: keys::mode_for_keys(self.term_mode(), self.config.kitty_protocol_on()),
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
        let Some(s) = self.session() else {
            return false;
        };
        // term first, then meta — the crate's lock order
        let alt = s.term.lock().mode().contains(TermMode::ALT_SCREEN);
        let (bidi_state, running) = {
            let m = s.meta.lock().unwrap();
            (m.bidi.clone(), m.running_cmd.clone())
        };
        // Idle explicit mode (a command that forgot to reset) must not
        // suppress restoration at the prompt. A live explicit mode still does.
        let (reorder, _) =
            bidi_state.for_display(self.config.honor_bdsm(), !running.is_empty(), alt);
        if !reorder {
            return false;
        }
        if !self.auto_follow {
            return self.claude_manual;
        }
        bidi::cmd_is_claude(&running)
    }

    /// Pixel position -> (pane index, viewport row, col, cell side).
    /// None inside the tab bar, the padding, or a divider gap.
    fn pane_cell_at(&self, pos: PhysicalPosition<f64>) -> Option<(usize, usize, usize, Side)> {
        let r = self.renderer.as_ref()?;
        let hit = mouse::cell_at(
            pos.x,
            pos.y,
            &self.pane_rects(),
            r.cell_w as f64,
            r.cell_h as f64,
        )?;
        let side = if hit.left { Side::Left } else { Side::Right };
        if let Some(rect) = self.pane_rects().get(hit.pane) {
            if let Some((col, side)) = r.hit_cell(*rect, hit.row, pos.x) {
                return Some((hit.pane, hit.row, col, side));
            }
        }
        Some((hit.pane, hit.row, hit.col, side))
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
            self.update_title();
            self.request_redraw();
        }
    }

    /// Viewport cell under `pos`, with a bidi row's column mapped back to
    /// the grid cell the program stored. `hit` is the raw grid cell.
    fn finish_locate(
        &mut self,
        hit: mouse::CellHit,
        pos: PhysicalPosition<f64>,
        rects: &[render::Rect],
    ) -> Option<(u64, usize, usize)> {
        // copy what the locks need, then drop the pane borrow so the term
        // lock below can re-borrow the tab
        let (pane_id, running, bidi_state, evicted, claude_from) = {
            let pane = self.tabs.get(self.active)?.panes.get(hit.pane)?;
            let m = pane.session.meta.lock().unwrap();
            (
                pane.id,
                m.running_cmd.clone(),
                m.bidi.clone(),
                m.evicted,
                m.claude_from_abs,
            )
        };
        let (auto, manual) = (self.auto_follow, self.claude_manual);
        let honor = self.config.honor_bdsm();
        let (cols, row, cells, claude, vis, reorder, dir) = {
            let t = self.tabs[self.active].panes[hit.pane].session.term.lock();
            let cols = t.columns().max(1);
            let rows = t.screen_lines().max(1);
            let row = hit.row.min(rows - 1);
            // Explicit BDSM draws Arabic on the grid, so the click column
            // is x / cell_w. Implicit mode uses the same plan as the painter,
            // SCP mark included. Neither applies once the command has ended
            // and the pane is back on the primary screen.
            let alt = t.mode().contains(TermMode::ALT_SCREEN);
            let (reorder, use_stamps) = bidi_state.for_display(honor, !running.is_empty(), alt);
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
            let dir = if use_stamps {
                bidi_state.direction_at(bidi_ctl::viewport_line_abs(
                    evicted, history, off, row as u64,
                ))
            } else {
                bidi_ctl::ParagraphDir::Default
            };
            let cells = render::snapshot_row(&t, row);
            (
                cols,
                row,
                cells,
                claude,
                hit.col.min(cols - 1),
                reorder,
                dir,
            )
        };
        let mapped = rects
            .get(hit.pane)
            .and_then(|rect| {
                self.renderer
                    .as_ref()
                    .and_then(|r| r.hit_cell(*rect, row, pos.x))
            })
            .map(|h| h.0);
        let col = if let Some(col) = mapped {
            col
        } else if reorder && cells.iter().any(|c| render::is_arabic(c.c)) {
            let rect = rects.get(hit.pane).copied().unwrap_or((0, 0, 1, 1));
            let x = (pos.x - rect.0 as f64) as f32;
            self.renderer
                .as_mut()
                .and_then(|r| r.logical_column_for(&cells, x, rect.2, claude, dir, cols))
                .unwrap_or(vis)
        } else {
            vis
        };
        Some((pane_id, row, col.min(cols - 1)))
    }

    /// The cell under the pointer, or None when it isn't over a pane.
    fn locate(&mut self, pos: PhysicalPosition<f64>) -> Option<(usize, u64, usize, usize)> {
        let rects = self.pane_rects();
        let (cw, ch) = {
            let r = self.renderer.as_ref()?;
            (r.cell_w as f64, r.cell_h as f64)
        };
        let hit = mouse::cell_at(pos.x, pos.y, &rects, cw, ch)?;
        let pane = hit.pane;
        let (id, row, col) = self.finish_locate(hit, pos, &rects)?;
        Some((pane, id, row, col))
    }

    /// Same mapping, but clamped into `pane_id`'s rect so a drag that
    /// leaves the pane still reports a cell of that pane.
    fn cell_in_pane_clamped(
        &mut self,
        pane_id: u64,
        pos: PhysicalPosition<f64>,
    ) -> Option<(usize, usize)> {
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
        let (_, row, col) = self.finish_locate(hit, PhysicalPosition::new(cx, cy), &rects)?;
        Some((row, col))
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
            .lock()
            .unwrap()
            .cwd
            .clone();
        link::cwd_hook(raw.as_deref())
    }

    fn links_on(&mut self, id: u64, row: usize) -> Vec<link::LinkSpan> {
        if let Some(spans) = self.link_cache.get(id, row) {
            return spans;
        }
        let Some((ti, pi)) = self.find_pane(id) else {
            return Vec::new();
        };
        let spans = {
            let term = self.tabs[ti].panes[pi].session.term.lock();
            link::links_on_row(&term, row)
        };
        self.link_cache.insert(id, row, spans.clone());
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
    fn refresh_link_hover(&mut self, pos: PhysicalPosition<f64>) -> Option<(u64, usize, usize)> {
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
        match span.activate(cwd.as_deref(), self.config.editor.as_deref(), None) {
            action @ (link::Action::Browse(_)
            | link::Action::OpenPath(_)
            | link::Action::Reveal(_)
            | link::Action::Edit { .. }
            | link::Action::EditDefault { .. }) => self.spawn_link_open(action),
            link::Action::Confirm { uri, host } => {
                self.blocked_link = Some(BlockedLink {
                    reason: link::HOST_CONFIRM.to_string(),
                    shown: link::confirm_card_shown(&host, &uri),
                    confirm_uri: Some(uri),
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
        let spawned = std::thread::Builder::new()
            .name("bayan-link".into())
            .spawn(move || {
                link::init_link_thread();
                let action = link::settle_live(action);
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

    fn refuse_link(&mut self, reason: link::RefuseReason, shown: &str) {
        self.blocked_link = Some(BlockedLink {
            reason: reason.label().to_string(),
            shown: link::preview(shown, 160),
            confirm_uri: None,
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
        Some((pane, mouse::right_click(overlay, tracking, shift, sel, cos)))
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
            .map(|(row, col)| mouse::Cell {
                pane: pane_id,
                row,
                col,
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
    fn copy_selection(&mut self, clear: bool) -> bool {
        let Some(session) = self.session() else {
            return false;
        };
        let text = {
            let mut t = session.term.lock();
            let txt = t.selection_to_string();
            if clear {
                t.selection = None;
            }
            txt
        };
        match text {
            Some(s) if !s.is_empty() => {
                // Claude rows hold VISUAL-order Arabic: restore logical so a
                // paste elsewhere reads correctly (M7 gap closed)
                let s = if self.claude_active() {
                    bidi::restore_block(&s)
                } else {
                    s
                };
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
        let (lines, chars, reason) = if let Some(g) = term::needs_paste_guard(&txt) {
            (g.lines, g.chars, g.risk.label().to_string())
        } else if let Some(risk) = fallback {
            (1, txt.chars().count(), risk.label().to_string())
        } else {
            self.send_paste(&txt);
            return;
        };
        self.pending_paste = Some(PendingPaste {
            text: txt,
            lines,
            chars,
            reason,
        });
        self.request_redraw();
    }

    fn send_paste(&mut self, txt: &str) {
        let bracketed = self.term_mode().contains(TermMode::BRACKETED_PASTE);
        let body = term::normalize_paste(txt, bracketed);
        self.last_input = Instant::now(); // pasting is typing, blink-wise
        if let Some(s) = self.session_mut() {
            s.write(body.as_bytes());
        }
    }

    fn scroll_lines(&mut self, n: i32) {
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
                if let (Some(st), Some(t)) = (self.search.as_mut(), event.text.as_ref()) {
                    for ch in t.chars() {
                        if !ch.is_control() {
                            st.query.push(ch);
                            st.hl = None;
                            st.selected_needle = None;
                        }
                    }
                }
            }
        }
        self.request_redraw();
    }

    fn redraw(&mut self) {
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
        if let Some(r) = self.renderer.as_mut() {
            r.set_bar_hidden(hide);
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
        let palette_view: Option<(String, Vec<String>, usize)> = self.palette.as_ref().map(|p| {
            let rows: Vec<String> = self
                .palette_rows(&p.query)
                .into_iter()
                .map(|(_, l)| l)
                .collect();
            let sel = p.sel.min(rows.len().saturating_sub(1));
            (p.query.clone(), rows, sel)
        });
        let close_msg: Option<String> = self.pending_close.map(|target| match target {
            CloseTarget::Pane(ti, pi) => {
                let cmd = self.running_command(ti, pi).unwrap_or_default();
                format!("الأمر «{cmd}» ما يزال يعمل — إغلاق اللوحة؟")
            }
            CloseTarget::Window => {
                format!("{} أمر ما يزال يعمل — إغلاق بيان؟", self.running_count())
            }
        });
        let rects = self.pane_rects();
        let tab_infos: Vec<render::TabInfo> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| render::TabInfo {
                title: t.title.clone(),
                busy: t.busy(i == self.active),
                attention: t.attention,
                active: i == self.active,
                progress: t.progress(),
            })
            .collect();
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let px = window.inner_size();
        if px.width == 0 || px.height == 0 {
            return;
        }
        let mut verts: Vec<gpu::Vertex> = Vec::new();
        match (self.renderer.as_mut(), self.tabs.get(self.active)) {
            (Some(renderer), Some(tab)) if !rects.is_empty() => {
                renderer.begin_frame();
                let bordered = tab.panes.len() > 1;
                let (auto_follow, claude_manual) = (self.auto_follow, self.claude_manual);
                let mut focused_claude = false;
                for (pi, (pane, rect)) in tab.panes.iter().zip(&rects).enumerate() {
                    let focused = pi == tab.focused;
                    // meta snapshot BEFORE the term lock (lock order term>meta
                    // only applies when holding both; here they don't overlap)
                    let (marks, running_cmd, bidi_state, evicted, claude_from): PaneSnap = {
                        let m = pane.session.meta.lock().unwrap();
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
                        )
                    };
                    let t = pane.session.term.lock();
                    // Explicit BDSM suppresses reordering for the whole pane,
                    // Claude restoration included, and only while the command
                    // is running or the alt screen is up. honor_bdsm defaults on.
                    // The Arc is the frame's snapshot: cloning it does not copy
                    // the stamp list.
                    let alt = t.mode().contains(TermMode::ALT_SCREEN);
                    let (reorder, use_stamps) = bidi_state.for_display(
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
                    if focused {
                        focused_claude = claude_pane;
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
                    let (link_hover, link_tip) = match &self.link_hover {
                        Some(h) if h.pane_id == pane.id => {
                            (Some((h.row, h.start_col, h.end_col)), Some(&h.target))
                        }
                        _ => (None, None),
                    };
                    let view = render::PaneView {
                        rect: *rect,
                        focused,
                        cursor: cursor_style,
                        cursor_on,
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
                        line_dir: &line_dir,
                        search_match: if focused {
                            self.search.as_ref().and_then(|s| s.hl.as_ref())
                        } else {
                            None
                        },
                        marks: &marks,
                        link_hover,
                        link_tip,
                    };
                    renderer.draw_pane(&mut verts, &view, &t);
                }
                renderer.use_ui_metrics(true);
                renderer.draw_chrome(
                    &mut verts,
                    px.width as usize,
                    px.height as usize,
                    &tab_infos,
                    self.search.as_ref().map(|s| s.query.as_str()),
                    focused_claude,
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
                            font_family: &settings_family,
                            font_size: settings_size,
                            cursor: cursor_style,
                            cursor_blink: self.config.cursor_blink_on(),
                            scrollback: self.config.scrollback_lines(),
                            copy_on_select: self.config.copy_on_select_on(),
                            ligatures: self.config.ligatures.unwrap_or(true),
                            bell: self.config.bell_mode(),
                            notifications: self.config.notifications_on(),
                            padding: self.config.padding_px(),
                            opacity_pct: (self.config.opacity_level() * 100.0).round() as i32,
                            shell: &shells::label_for(&self.config.shell_program()),
                            hide_single_tab: self.config.hide_single_tab_on(),
                            confirm_close: self.config.confirm_close_on(),
                        },
                    );
                }
                if let Some(p) = &self.pending_paste {
                    let preview = term::paste_preview(&p.text, 48);
                    renderer.draw_paste_guard(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        &render::PasteCard {
                            lines: p.lines,
                            chars: p.chars,
                            reason: &p.reason,
                            preview: &preview,
                        },
                    );
                }
                if let Some(block) = &self.blocked_link {
                    renderer.draw_link_block(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        &block.reason,
                        &block.shown,
                        block.confirm_uri.is_some(),
                    );
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
                if let Some((q, rows, sel)) = &palette_view {
                    renderer.draw_palette(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        q,
                        rows,
                        *sel,
                    );
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
        let Some(gpu) = self.gpu.as_mut() else { return };
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
        if std::env::var_os("BAYAN_ATLAS_STRESS").is_some() {
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
        // never skip the very first present (it reveals the hidden window)
        let unchanged = self.first_frame && !heal && frame_bytes == self.last_frame.as_slice();
        if unchanged {
            return;
        }
        self.last_frame.clear();
        self.last_frame.extend_from_slice(frame_bytes);
        let bg = self.renderer.as_ref().map_or(render::BG, |r| r.bg);
        gpu.render(&verts, bg);
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
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // hidden until the GPU presents the first dark frame (~0.4s): a
        // fully-drawn window appearing beats a white flash while DX12 warms up.
        // BAYAN_SHOW_NOW forces it visible at creation (for on-screen capture).
        let visible = std::env::var_os("BAYAN_SHOW_NOW").is_some();
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
            .with_title("Bayan — بيان")
            .with_window_icon(icon)
            .with_visible(visible)
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
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        crate::prof::mark("window created");
        let px = window.inner_size();
        let gpu = gpu::Gpu::new(window.clone(), px.width, px.height, render::ATLAS_SIZE)
            .expect("gpu init (DX12/Vulkan/GL via wgpu)");
        crate::prof::mark("gpu ready");
        self.gpu = Some(gpu);
        self.window = Some(window.clone());
        self.apply_opacity(); // the configured window alpha, from frame one
        self.apply_window_frame();
        if let Some(h) = self.hwnd() {
            // the tray icon: toast anchor + summon target
            if !toast::init(h) {
                eprintln!("bayan: tray icon registration failed — no toasts");
            }
        }
        // first frame NOW: a dark window on screen beats a frozen launcher.
        // The shell and the font system warm up behind it, in parallel.
        self.redraw();
        // the PTY + conhost + PowerShell chain is the slowest dependency:
        // start it immediately with a default grid; the exact grid follows
        // once cell metrics exist (EasyTer starts 110x32 the same way).
        // Start one fresh tab unless saved-tab restoration is explicitly enabled.
        self.restore_state();
        // debug hook: BAYAN_SPLIT=1 opens the first tab pre-split, so the
        // pane machinery can be verified without injecting any input
        if std::env::var_os("BAYAN_SPLIT").is_some() {
            self.split(Orientation::Row);
        }
        if std::env::var_os("BAYAN_COCKPIT").is_some() {
            self.cockpit = true;
            self.cockpit_sel = self.active;
        }
        if std::env::var_os("BAYAN_GUARD").is_some() {
            self.arm_pending_paste("echo one\necho two\necho three\n".into(), None);
        }
        if std::env::var_os("BAYAN_SETTINGS").is_some() {
            self.settings = Some(0);
        }
        if std::env::var_os("BAYAN_SHORTCUTS").is_some() {
            self.shortcuts = Some(ShortcutsState::default());
        }
        // BAYAN_TYPE=<text>: feed text into the first pane's PTY — the
        // visual-verification hook (ligatures, shaping) with no keyboard
        if let Some(v) = std::env::var_os("BAYAN_TYPE") {
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
        if let Some(v) = std::env::var_os("BAYAN_PASTE") {
            self.debug_paste = v.to_str().map(str::to_string);
        }
        // BAYAN_POSH=1: open the prompt picker at startup (capture without keys)
        if std::env::var_os("BAYAN_POSH").is_some() {
            self.open_posh();
        }
        // BAYAN_PALETTE=<query|1>: open the palette at startup, so it can be
        // captured without injecting keystrokes (the debug-hook rule)
        if let Some(v) = std::env::var_os("BAYAN_PALETTE") {
            let q = v.to_str().unwrap_or("").to_string();
            self.open_palette(if q == "1" { String::new() } else { q });
        }
        // BAYAN_TOAST=<title|1>: fire a sample toast at startup (M19
        // verification; a custom value becomes the title, so direction
        // experiments run without rebuilding)
        if let Some(v) = std::env::var_os("BAYAN_TOAST") {
            if let Some(h) = self.hwnd() {
                let t = v.to_str().unwrap_or("1");
                let title = if t == "1" { "اكتمل الأمر" } else { t };
                let ok = toast::show(h, title, "بيان — إشعار تجريبي (M19)");
                eprintln!("bayan: BAYAN_TOAST show accepted={ok}");
            }
        }
        // BAYAN_PICK_THEME=<n>: open settings and apply theme n, so a click's
        // live effect can be verified in a screenshot without input injection
        if let Some(v) = std::env::var_os("BAYAN_PICK_THEME") {
            self.settings = Some(0);
            if let Some(n) = v.to_str().and_then(|s| s.parse::<usize>().ok()) {
                self.apply_theme(n);
            }
        }
        crate::prof::mark("session spawned");
        // the window exists now; if it is already focused and a pane has
        // 1004, say so. has_focus is the same check a later rebase uses.
        self.sync_focus(None);
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
                            format!("تبويب جديد — {}", shell.name),
                        )
                    }));
                }
                self.shell_profiles = profiles;
                self.request_redraw();
            }
            UserEvent::Wakeup(id) => {
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
                    let ris = {
                        let mut meta = self.tabs[ti].panes[pi].session.meta.lock().unwrap();
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
            UserEvent::PtyWrite(id, text) => {
                if let Some((ti, pi)) = self.find_pane(id) {
                    self.tabs[ti].panes[pi].session.write(text.as_bytes());
                }
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
                        let body = self.tabs[ti].title.clone();
                        self.toast_from_pty("بيان — تنبيه", &body);
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
                    if (background || !self.focused) && self.config.notifications_on() {
                        let body = if body.is_empty() {
                            self.tabs[ti].title.clone()
                        } else {
                            body
                        };
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
                            let title = if ok {
                                "اكتمل الأمر"
                            } else {
                                "فشل الأمر"
                            };
                            let body = self.tabs[ti].title.clone();
                            self.toast_from_pty(title, &body);
                        }
                        self.request_redraw();
                    }
                }
            }
            UserEvent::Cwd(id, cwd) => {
                let base = cwd.rsplit(['\\', '/']).next().unwrap_or(&cwd).to_string();
                if let Some((ti, pi)) = self.find_pane(id) {
                    // the tab is named after its FOCUSED pane's directory
                    if self.tabs[ti].focused == pi {
                        self.tabs[ti].title = base;
                        if ti == self.active {
                            self.update_title();
                        }
                    }
                }
                self.request_redraw(); // tab titles live in the frame
            }
            UserEvent::RendererReady(r) => {
                crate::prof::mark("renderer ready");
                self.renderer = Some(*r);
                self.renderer_building = false;
                // now that cell metrics exist, snap every pane to its rect
                self.relayout_active();
                // the zoom moved again while this build ran: chase it
                if (self.font_delta - self.inflight_delta).abs() > f32::EPSILON {
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
            UserEvent::LinkFailed { reason, shown } => {
                self.blocked_link = Some(BlockedLink {
                    reason,
                    shown: link::preview(&shown, 160),
                    confirm_uri: None,
                });
                self.clear_link_hover();
                self.request_redraw();
            }
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
        match wake {
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
            WindowEvent::CursorLeft { .. } => self.clear_link_hover(),
            WindowEvent::MouseInput { state, button, .. } => self.mouse_input(state, button, el),
            WindowEvent::MouseWheel { delta, .. } => self.mouse_wheel(delta),
            WindowEvent::KeyboardInput { event, .. } => self.key_input(&event, el),
            WindowEvent::RedrawRequested => self.redraw(),
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
pub(crate) fn harden_dll_search() {
    // An empty string removes the CWD. NULL would put it back.
    let empty = [0u16];
    // SAFETY: `empty` is a NUL-terminated UTF-16 string that outlives the
    // call, and SetDllDirectoryW copies it rather than keeping the pointer.
    unsafe {
        let _ = windows_sys::Win32::System::LibraryLoader::SetDllDirectoryW(empty.as_ptr());
    }
}

fn main() {
    harden_dll_search();
    prof::init();
    prof::mark("main start");
    use winit::platform::windows::EventLoopBuilderExtWindows;
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .with_msg_hook(|msg| {
            use windows_sys::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY};
            // SAFETY: winit's msg hook is handed a pointer to the MSG being
            // dispatched, valid for the duration of the callback. It is only read.
            let m = unsafe { &*(msg as *const MSG) };
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
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    prof::mark("event loop built");
    let proxy = event_loop.create_proxy();
    let profiles_proxy = proxy.clone();
    std::thread::spawn(move || {
        let _ = profiles_proxy.send_event(UserEvent::ShellProfiles(shells::detect()));
    });
    let cfg = config::load();
    // plugins load once, before the first renderer, so a plugin theme named in
    // the config resolves on the very first frame
    let plug = plugins::init(&cfg.disabled_plugins());
    if !plug.failed.is_empty() {
        eprintln!(
            "bayan: {} plugin(s) failed — see {}",
            plug.failed.len(),
            config::bayan_dir().join("plugins.log").display()
        );
    }
    let keymap = keybinds::effective_map(&cfg);
    let mut app = App {
        posh: None,
        last_toast: None,
        mru: mru::Mru::new(),
        palette: None,
        shell_profiles: Vec::new(),
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
        cursor_pos: PhysicalPosition::new(0.0, 0.0),
        mouse: mouse::Route::default(),
        sel_pane: 0,
        drag_divider: None,
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
        search: None,
        clipboard: None,
        auto_follow: true,
        claude_manual: false,
        config: cfg,
        keymap,
        shortcuts: None,
        atlas_generation: 0,
        font_delta: 0.0,
        renderer_building: false,
        inflight_delta: 0.0,
        normal_geom: None,
        keys_held_away: Vec::new(),
        debug_paste: None,
    };
    event_loop.run_app(&mut app).expect("run event loop");
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
    fn split_rects_divide_by_weight() {
        let content = (0, 30, 1000, 700);
        let one = split_rects(content, &[1.0], Orientation::Row);
        assert_eq!(one, vec![(0, 30, 1000, 700)]);
        let two = split_rects(content, &[1.0, 1.0], Orientation::Row);
        assert_eq!(two[0], (0, 30, 499, 700));
        assert_eq!(two[1], (500, 30, 500, 700)); // absorbs the rounding
        assert_eq!(two[0].0 + two[0].2 + 1, two[1].0); // 1px divider gap
                                                       // a dragged divider: 3:1 weights give a 3:1 width split
        let uneven = split_rects(content, &[3.0, 1.0], Orientation::Row);
        assert_eq!(uneven[0].2, 749); // 999 * 0.75
        assert_eq!(uneven[1].0 + uneven[1].2, 1000); // flush right
        let stacked = split_rects(content, &[1.0, 1.0, 1.0], Orientation::Column);
        assert_eq!(stacked.len(), 3);
        assert_eq!(stacked[2].1 + stacked[2].3, 30 + 700); // flush bottom
                                                           // widths untouched in a column split
        assert!(stacked.iter().all(|r| r.2 == 1000));
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
        let mut meta = term::SessionMeta::default();
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
            b"\x1b]133;A\x07> ok\r\n",
        );
        feed(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;D;0\x07");
        feed(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]133;A\x07> bad\r\n",
        );
        feed(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;D;1\x07");
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
            }],
            active: 0,
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
        assert_eq!(active_after_remove(2, 0, 3), 1);
        // closing a higher tab leaves the index alone
        assert_eq!(active_after_remove(1, 3, 3), 1);
        // closing the active tab itself: clamp to the last remaining
        assert_eq!(active_after_remove(2, 2, 2), 1);
        assert_eq!(active_after_remove(0, 0, 2), 0);
        assert_eq!(active_after_remove(5, 0, 0), 0);
    }

    /// The wheel inside a TUI: arrows unless the app tracks the mouse, and
    /// then the encoding IT asked for — an SGR report to a program that only
    /// enabled X10 tracking is as much garbage as one to a program with none.
    #[test]
    fn wheel_sends_what_the_tui_asked_for() {
        let none = TermMode::ALT_SCREEN;
        assert_eq!(wheel_seq(none, 1, 5, 7), b"\x1b[A");
        assert_eq!(wheel_seq(none, -1, 5, 7), b"\x1b[B");
        assert_eq!(wheel_seq(none | TermMode::APP_CURSOR, 1, 5, 7), b"\x1bOA");
        let x10 = none | TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(wheel_seq(x10, 1, 5, 7), b"\x1b[M\x60\x27\x25");
        let sgr = x10 | TermMode::SGR_MOUSE;
        assert_eq!(wheel_seq(sgr, -1, 5, 7), b"\x1b[<65;7;5M");
        // X10 is ONE byte per value: column 200 is 0xE8, never a two-byte
        // UTF-8 pair, and anything past 223 clamps rather than wrapping
        assert_eq!(wheel_seq(x10, 1, 3, 200), b"\x1b[M\x60\xe8\x23");
        assert_eq!(wheel_seq(x10, 1, 500, 900).len(), 6);
        assert_eq!(wheel_seq(x10, 1, 500, 900)[4], 255);
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
