//! Bayan (بيان) — an Arabic-first, agent-ready terminal.
//!
//! Milestone M1: one window, one PowerShell over ConPTY, correct Arabic
//! shaping and BiDi from day one. Architecture mirrors EasyTer (Python/Qt),
//! whose regression suites serve as the behavioral specification.

// release builds are pure GUI apps: no companion console window
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bidi;
mod shells;
mod config;
mod gpu;
mod keybinds;
mod keys;
mod toast;
mod render;
mod term;

/// rgb -> "#rrggbb" for the config file.
fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

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

/// The system notification sound (bell mode "sound"). Async — never blocks
/// the UI thread.
fn beep() {
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
            KeyA => 'a', KeyB => 'b', KeyC => 'c', KeyD => 'd', KeyE => 'e',
            KeyF => 'f', KeyG => 'g', KeyH => 'h', KeyI => 'i', KeyJ => 'j',
            KeyK => 'k', KeyL => 'l', KeyM => 'm', KeyN => 'n', KeyO => 'o',
            KeyP => 'p', KeyQ => 'q', KeyR => 'r', KeyS => 's', KeyT => 't',
            KeyU => 'u', KeyV => 'v', KeyW => 'w', KeyX => 'x', KeyY => 'y',
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
            let p = if matches!(dir, Direction::Right) { *m.end() } else { *m.start() };
            step_point(p, cols, top, bottom, dir)
        }
        None => Point::new(Line(bottom), Column(cols.saturating_sub(1))),
    };
    let side = if matches!(dir, Direction::Right) { Side::Left } else { Side::Right };
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
        marks.iter().map(|&a| a as i64).filter(|&a| a < abs_top).max()
    } else {
        marks.iter().map(|&a| a as i64).filter(|&a| a > abs_top).min()
    }?;
    Some((hist as i64 - target + 1).clamp(0, hist as i64) as usize)
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
        let line = format!("{:9.1} ms  {}\n", t0.elapsed().as_secs_f64() * 1000.0, label);
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
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::TermMode;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use term::{Session, TermSize};

pub enum UserEvent {
    /// New PTY output was parsed in tab `id`: repaint (and busy-dot it).
    Wakeup(u64),
    /// Tab `id`'s emulator answers its child (DSR/DA replies TUIs block on).
    PtyWrite(u64, String),
    /// The font system finished loading on its background thread.
    RendererReady(Box<render::Renderer>),
    /// A program set the clipboard via OSC 52 (yank over SSH ...).
    ClipboardSet(String),
    /// Pane `id`'s shell reported its working directory (OSC 9;9).
    Cwd(u64, String),
    /// Pane `id` rang the bell (Claude waiting for an approval).
    Bell(u64),
    /// A command that ran ≥6s in pane `id` just finished (ok?).
    CommandFinished(u64, bool),
    /// Pane `id`'s shell exited.
    Exit(u64),
}

/// One terminal pane (a tab holds one or more).
struct Pane {
    id: u64,
    session: Session,
    /// when this pane last produced output — feeds the busy dot
    last_output: Option<Instant>,
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
    })
}

fn state_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE").ok()?;
    Some(std::path::Path::new(&home).join(".bayan").join("session.json"))
}

/// Multi-click detection: 1 = simple, 2 = word (semantic), 3 = line.
struct ClickTracker {
    last: Option<(Instant, (usize, usize))>,
    count: u8,
}

impl ClickTracker {
    fn new() -> Self {
        Self { last: None, count: 0 }
    }

    fn click(&mut self, now: Instant, cell: (usize, usize)) -> u8 {
        const DOUBLE_MS: u128 = 400;
        self.count = match self.last {
            Some((t, c)) if now.duration_since(t).as_millis() <= DOUBLE_MS && c == cell => {
                if self.count >= 3 { 1 } else { self.count + 1 }
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
}

struct App {
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
    mouse_left_down: bool,
    /// the pane a mouse selection started in (drags don't cross panes)
    sel_pane: usize,
    /// divider being dragged (between pane i and i+1)
    drag_divider: Option<usize>,
    /// the agent cockpit overlay (Ctrl+Shift+D): one glance at every tab
    cockpit: bool,
    cockpit_sel: usize,
    /// a big paste awaiting Enter/Esc (EasyTer's paste guard)
    pending_paste: Option<String>,
    /// a close awaiting Enter/Esc (a command is still running)
    pending_close: Option<CloseTarget>,
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

    /// Index of the active theme (which the panel highlights).
    fn active_theme(&self) -> usize {
        self.config
            .theme
            .as_deref()
            .and_then(|n| render::THEMES.iter().position(|t| t.name == n))
            .unwrap_or(0)
    }

    /// Apply theme `idx` (copy its colors into the config), live.
    fn apply_theme(&mut self, idx: usize) {
        if let Some(t) = render::THEMES.get(idx) {
            self.config.theme = Some(t.name.to_string());
            self.config.bg = Some(hex(t.bg));
            self.config.fg = Some(hex(t.fg));
            self.config.palette = Some(t.palette.iter().map(|&c| hex(c)).collect());
            self.rebuild_renderer();
            self.request_redraw();
        }
    }

    fn change_font_size(&mut self, delta: i32) {
        let s = (self.config.font_size.unwrap_or(15.0) + delta as f32).clamp(8.0, 40.0);
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
        let Some(r) = self.renderer.as_ref() else { return };
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
        let i = steps.iter().position(|&s| s >= cur).unwrap_or(steps.len() - 1) as i32;
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
        let i = steps.iter().position(|&s| s >= cur).unwrap_or(steps.len() - 1) as i32;
        let j = (i + dir).clamp(0, steps.len() as i32 - 1) as usize;
        self.config.padding = Some(steps[j]);
        self.relayout_active(); // pane rects (and PTY sizes) follow the inset
        self.request_redraw();
    }

    fn change_opacity(&mut self, dir: i32) {
        let cur = (self.config.opacity_level() * 100.0).round() as u32;
        let steps = config::OPACITY_STEPS;
        let i = steps.iter().position(|&s| s >= cur).unwrap_or(steps.len() - 1) as i32;
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

    fn apply_opacity(&self) {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Some(w) = &self.window else { return };
        let Ok(handle) = w.window_handle() else { return };
        let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
        let hwnd = h.hwnd.get() as windows_sys::Win32::Foundation::HWND;
        let alpha = (self.config.opacity_level() * 255.0).round() as u8;
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW,
                GWL_EXSTYLE, LWA_ALPHA, WS_EX_LAYERED,
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

    /// Run one rebindable action (the keymap's dispatch target).
    fn perform_action(&mut self, action: keybinds::Action, el: &ActiveEventLoop) {
        use keybinds::Action::*;
        match action {
            NewTab => {
                // new tab inherits the active tab's directory
                let cwd = self
                    .session()
                    .and_then(|s| s.meta.lock().unwrap().cwd.clone());
                self.spawn_tab(cwd);
                self.update_title();
                self.request_redraw();
            }
            ClosePane => {
                let (ti, pi) = (
                    self.active,
                    self.tabs.get(self.active).map_or(0, |t| t.focused),
                );
                if self.config.confirm_close_on() && self.running_command(ti, pi).is_some()
                {
                    self.pending_close = Some(CloseTarget::Pane(ti, pi));
                    self.request_redraw();
                    return;
                }
                self.remove_pane(ti, pi, el);
            }
            Search => {
                self.search = Some(SearchState::default());
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
        }
    }

    /// Keyboard while the shortcuts editor is open: arrows select, Enter
    /// captures, Delete restores a default, Esc closes (or cancels a
    /// capture). A capture takes the next bindable chord.
    fn shortcuts_input(&mut self, event: &KeyEvent, el: &ActiveEventLoop) {
        let _ = el;
        let Some(st) = self.shortcuts.as_mut() else { return };
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
            let Some(chord) = keybinds::chord_from(event.physical_key, ctrl, shift, alt)
            else {
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
                    st.flash =
                        Some(format!("«{}» يستخدم هذا الاختصار", owner.label()));
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
        let Some(lay) = self.renderer.as_ref().map(|r| r.shortcuts_layout(fw, fh, n))
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
        let next_flip = self.last_input
            + std::time::Duration::from_millis(((k + 1) * BLINK_MS) as u64);
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
        } else if let Some(i) =
            lay.theme_tiles.iter().position(|&t| render::rect_hit(t, px, py))
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
        } else if let Some(i) =
            lay.cursor_btns.iter().position(|&b| render::rect_hit(b, px, py))
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
        } else if let Some(i) =
            lay.bell_btns.iter().position(|&b| render::rect_hit(b, px, py))
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
        r.cockpit_row_at(px.width as usize, px.height as usize, self.tabs.len(),
                         pos.x, pos.y)
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
                self.cockpit_sel =
                    (self.cockpit_sel + 1).min(self.tabs.len().saturating_sub(1));
            }
            _ => {}
        }
        self.request_redraw();
    }

    /// Scroll to the previous (dir<0) or next command prompt (OSC 133 marks).
    fn jump_command(&mut self, dir: i32) {
        let Some(s) = self.session() else { return };
        {
            let mut t = s.term.lock().unwrap();
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
        let id = self.next_id;
        self.next_id += 1;
        match Session::spawn(self.size, self.proxy.clone(), id, cwd,
                             self.config.scrollback_lines(),
                             &self.config.shell_program()) {
            Ok(session) => Some(Pane { id, session, last_output: None }),
            Err(e) => {
                eprintln!("bayan: spawn failed: {e}");
                None
            }
        }
    }

    fn spawn_tab(&mut self, cwd: Option<String>) {
        let title = cwd
            .as_deref()
            .and_then(|c| c.rsplit(['\\', '/']).next().map(str::to_string))
            .unwrap_or_else(|| "بيان".to_string());
        if let Some(pane) = self.spawn_pane(cwd.as_deref()) {
            self.tabs.push(Tab {
                panes: vec![pane],
                weights: vec![1.0],
                focused: 0,
                orientation: Orientation::Row,
                title,
                attention: false,
            });
            self.active = self.tabs.len() - 1;
            self.relayout_active();
        }
    }

    /// Pane rects for the active tab within the window's content area.
    fn pane_rects(&self) -> Vec<render::Rect> {
        let (Some(r), Some(w), Some(t)) =
            (self.renderer.as_ref(), self.window.as_ref(), self.tabs.get(self.active))
        else {
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

    /// Divider index under the pointer (between pane i and i+1), if any.
    fn divider_at(&self, pos: PhysicalPosition<f64>) -> Option<usize> {
        let t = self.tabs.get(self.active)?;
        if t.panes.len() < 2 {
            return None;
        }
        let rects = self.pane_rects();
        const GRAB: f64 = 4.0;
        for i in 0..rects.len() - 1 {
            let (x, y, w, h) = rects[i];
            let hit = match t.orientation {
                Orientation::Row => {
                    (pos.x - (x + w) as f64).abs() <= GRAB
                        && pos.y >= y as f64
                        && pos.y < (y + h) as f64
                }
                Orientation::Column => {
                    (pos.y - (y + h) as f64).abs() <= GRAB
                        && pos.x >= x as f64
                        && pos.x < (x + w) as f64
                }
            };
            if hit {
                return Some(i);
            }
        }
        None
    }

    /// Drag divider `i` to the pointer: redistribute the PAIR's weight.
    fn drag_divider_to(&mut self, i: usize, pos: PhysicalPosition<f64>) {
        let rects = self.pane_rects();
        let Some(t) = self.tabs.get_mut(self.active) else { return };
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
        // resolve "hide the bar with one tab" BEFORE measuring the content
        let hide = self.config.hide_single_tab_on() && self.tabs.len() <= 1;
        if let Some(r) = self.renderer.as_mut() {
            r.set_bar_hidden(hide);
        }
        let rects = self.pane_rects();
        let Some(r) = self.renderer.as_ref() else { return };
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
        if self.tabs.get(self.active).is_none_or(|t| t.panes.len() >= 4) {
            return;
        }
        let cwd = self
            .session()
            .and_then(|s| s.meta.lock().unwrap().cwd.clone());
        let Some(pane) = self.spawn_pane(cwd.as_deref()) else { return };
        if let Some(t) = self.tabs.get_mut(self.active) {
            if t.panes.len() == 1 {
                t.orientation = orientation;
            }
            t.panes.push(pane);
            t.weights.push(1.0);
            t.focused = t.panes.len() - 1;
        }
        self.relayout_active();
        self.request_redraw();
    }

    /// Remove one pane; a tab with no panes closes, the last tab closes Bayan.
    fn remove_pane(&mut self, ti: usize, pi: usize, el: &ActiveEventLoop) {
        let Some(t) = self.tabs.get_mut(ti) else { return };
        if pi >= t.panes.len() {
            return;
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
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            }
        } else if t.focused >= t.panes.len() {
            t.focused = t.panes.len() - 1;
        }
        self.relayout_active();
        self.update_title();
        self.request_redraw();
    }

    fn switch_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() || idx == self.active {
            return;
        }
        self.active = idx;
        if let Some(t) = self.tabs.get_mut(idx) {
            t.attention = false; // you're looking at it now
        }
        self.search = None; // the bar belongs to the tab that opened it
        self.relayout_active(); // this tab's layout may differ
        self.update_title();
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
                        .map(|p| p.session.meta.lock().unwrap().cwd.clone())
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
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(path, json);
        }
    }

    fn restore_state(&mut self) {
        let saved: Option<SavedState> = state_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| parse_state(&s));
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
            .map_or(TermMode::empty(), |s| *s.term.lock().unwrap().mode())
    }

    /// Claude mode: auto = a full-screen TUI is active AND the command that
    /// started it is Claude itself (vim/less emit logical Arabic — never
    /// reverse those). F2 overrides manually.
    fn claude_active(&self) -> bool {
        if !self.auto_follow {
            return self.claude_manual;
        }
        let Some(s) = self.session() else {
            return false;
        };
        if !self.term_mode().contains(TermMode::ALT_SCREEN) {
            return false;
        }
        bidi::cmd_is_claude(&s.meta.lock().unwrap().running_cmd)
    }

    /// Pixel position -> (pane index, viewport row, col, cell side).
    /// None inside the tab bar (that region belongs to tab switching).
    fn pane_cell_at(&self, pos: PhysicalPosition<f64>) -> Option<(usize, usize, usize, Side)> {
        let r = self.renderer.as_ref()?;
        let rects = self.pane_rects();
        let (pi, rect) = rects.iter().enumerate().find(|(_, (x, y, w, h))| {
            pos.x >= *x as f64
                && pos.x < (*x + *w) as f64
                && pos.y >= *y as f64
                && pos.y < (*y + *h) as f64
        })?;
        let (rx, ry, rw, rh) = *rect;
        let lx = pos.x - rx as f64;
        let ly = pos.y - ry as f64;
        let max_col = ((rw as f32 / r.cell_w) as usize).max(1) - 1;
        let max_row = ((rh as f32 / r.cell_h) as usize).max(1) - 1;
        let colf = lx / r.cell_w as f64;
        let col = (colf.floor().max(0.0) as usize).min(max_col);
        let row = ((ly / r.cell_h as f64).floor().max(0.0) as usize).min(max_row);
        let side = if colf - col as f64 <= 0.5 { Side::Left } else { Side::Right };
        Some((pi, row, col, side))
    }

    /// Tab index under a pixel position, if it's in the tab bar.
    fn tab_at(&self, pos: PhysicalPosition<f64>) -> Option<usize> {
        let r = self.renderer.as_ref()?;
        if pos.y >= r.tab_bar_h() as f64 {
            return None;
        }
        let idx = (pos.x / (r.cell_w * render::TAB_CELLS) as f64).floor() as usize;
        (idx < self.tabs.len()).then_some(idx)
    }

    fn pointer_cell_1based(&self) -> (usize, usize) {
        self.pane_cell_at(self.cursor_pos)
            .map(|(_, row, col, _)| (row + 1, col + 1))
            .unwrap_or((1, 1))
    }

    fn has_selection(&self) -> bool {
        self.session().is_some_and(|s| {
            s.term
                .lock()
                .unwrap()
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
            let mut t = session.term.lock().unwrap();
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

    fn set_clipboard(&mut self, s: String) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(c) = self.clipboard.as_mut() {
            let _ = c.set_text(s);
        }
    }

    fn paste(&mut self) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let Some(txt) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) else {
            return;
        };
        if txt.is_empty() {
            return;
        }
        // EasyTer's paste guard: confirm before a multi-line/huge paste
        if term::needs_paste_guard(&txt).is_some() {
            self.pending_paste = Some(txt);
            self.request_redraw();
            return;
        }
        self.send_paste(&txt);
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
            s.term.lock().unwrap().scroll_display(Scroll::Delta(n));
        }
        self.request_redraw();
    }

    /// Find the next search hit. Enter walks UP from the bottom (the match
    /// you can see is the one you want), Shift+Enter walks back down.
    fn search_step(&mut self, dir: Direction) {
        let query = match &self.search {
            Some(st) if !st.query.is_empty() => st.query.clone(),
            _ => return,
        };
        let Some(session) = self.session() else {
            return;
        };
        let found = {
            let mut t = session.term.lock().unwrap();
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
                self.search_step(if shift { Direction::Right } else { Direction::Left });
                return; // search_step already redraws
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(st) = self.search.as_mut() {
                    st.query.pop();
                    st.hl = None;
                }
            }
            _ => {
                if let (Some(st), Some(t)) = (self.search.as_mut(), event.text.as_ref()) {
                    for ch in t.chars() {
                        if !ch.is_control() {
                            st.query.push(ch);
                            st.hl = None;
                        }
                    }
                }
            }
        }
        self.request_redraw();
    }

    fn redraw(&mut self) {
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
        let settings_size = self.config.font_size.unwrap_or(15.0) as i32;
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
            })
            .collect();
        let Some(window) = self.window.as_ref() else { return };
        let px = window.inner_size();
        if px.width == 0 || px.height == 0 {
            return;
        }
        let mut verts: Vec<gpu::Vertex> = Vec::new();
        match (self.renderer.as_mut(), self.tabs.get(self.active)) {
            (Some(renderer), Some(tab)) if !rects.is_empty() => {
                let bordered = tab.panes.len() > 1;
                let (auto_follow, claude_manual) = (self.auto_follow, self.claude_manual);
                let mut focused_claude = false;
                for (pi, (pane, rect)) in tab.panes.iter().zip(&rects).enumerate() {
                    let focused = pi == tab.focused;
                    // meta snapshot BEFORE the term lock (lock order term>meta
                    // only applies when holding both; here they don't overlap)
                    let (marks, running_cmd): (Vec<(usize, Option<i32>)>, String) = {
                        let m = pane.session.meta.lock().unwrap();
                        (
                            // global -> grid space: subtract the evictions
                            m.marks
                                .iter()
                                .filter(|c| c.abs >= m.evicted)
                                .map(|c| ((c.abs - m.evicted) as usize, c.exit))
                                .collect(),
                            m.running_cmd.clone(),
                        )
                    };
                    let t = pane.session.term.lock().unwrap();
                    // Claude mode is a per-PANE fact: this pane's TUI + command
                    let claude_pane = if !auto_follow {
                        claude_manual
                    } else {
                        t.mode().contains(TermMode::ALT_SCREEN)
                            && bidi::cmd_is_claude(&running_cmd)
                    };
                    if focused {
                        focused_claude = claude_pane;
                    }
                    let view = render::PaneView {
                        rect: *rect,
                        focused,
                        cursor: cursor_style,
                        cursor_on,
                        bordered,
                        claude: claude_pane,
                        search_match: if focused {
                            self.search.as_ref().and_then(|s| s.hl.as_ref())
                        } else {
                            None
                        },
                        marks: &marks,
                    };
                    renderer.draw_pane(&mut verts, &view, &t);
                }
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
                            opacity_pct: (self.config.opacity_level() * 100.0).round()
                                as i32,
                            shell: &shells::label_for(&self.config.shell_program()),
                            hide_single_tab: self.config.hide_single_tab_on(),
                            confirm_close: self.config.confirm_close_on(),
                        },
                    );
                }
                if let Some(p) = &self.pending_paste {
                    if let Some((lines, chars)) = term::needs_paste_guard(p) {
                        renderer.draw_paste_guard(
                            &mut verts,
                            px.width as usize,
                            px.height as usize,
                            lines,
                            chars,
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
                if let Some(msg) = &close_msg {
                    renderer.draw_close_guard(
                        &mut verts,
                        px.width as usize,
                        px.height as usize,
                        msg,
                    );
                }
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
        let attrs = Window::default_attributes()
            .with_title("Bayan — بيان")
            .with_window_icon(icon)
            .with_visible(visible)
            .with_inner_size(LogicalSize::new(1100.0, 700.0));
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        crate::prof::mark("window created");
        let px = window.inner_size();
        let gpu = gpu::Gpu::new(window.clone(), px.width, px.height, render::ATLAS_SIZE)
            .expect("gpu init (DX12/Vulkan/GL via wgpu)");
        crate::prof::mark("gpu ready");
        self.gpu = Some(gpu);
        self.window = Some(window.clone());
        self.apply_opacity(); // the configured window alpha, from frame one
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
        // Restores yesterday's tabs (each in its saved cwd) or opens one.
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
            self.pending_paste = Some("echo one\necho two\necho three\n".into());
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
        // FontSystem::new scans every installed font — the documented
        // cosmic-text startup cost (pop-os/cosmic-text#247): keep it off
        // the UI thread and swap the renderer in when it's ready
        self.rebuild_renderer();
        // the quake hotkey (Ctrl+Alt+`): a null-hwnd registration posts
        // WM_HOTKEY to this thread's queue, where the msg hook catches it
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
            UserEvent::Wakeup(id) => {
                if !self.first_output {
                    self.first_output = true;
                    crate::prof::mark("first pty output");
                }
                if let Some((ti, pi)) = self.find_pane(id) {
                    self.tabs[ti].panes[pi].last_output = Some(Instant::now());
                }
                self.request_redraw();
            }
            UserEvent::PtyWrite(id, text) => {
                if let Some((ti, pi)) = self.find_pane(id) {
                    self.tabs[ti].panes[pi].session.write(text.as_bytes());
                }
            }
            UserEvent::ClipboardSet(text) => self.set_clipboard(text),
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
                            if let Some(h) = self.hwnd() {
                                let title =
                                    if ok { "اكتمل الأمر" } else { "فشل الأمر" };
                                toast::show(h, title, &self.tabs[ti].title);
                            }
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
                self.request_redraw();
            }
            UserEvent::Exit(id) => {
                // that pane's shell ended: close it; the last pane of the
                // last tab closes Bayan
                if let Some((ti, pi)) = self.find_pane(id) {
                    self.remove_pane(ti, pi, el);
                }
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
        match event {
            WindowEvent::CloseRequested => {
                // a running command deserves a second look before the kill
                if self.config.confirm_close_on()
                    && self.pending_close.is_none()
                    && self.running_count() > 0
                {
                    self.pending_close = Some(CloseTarget::Window);
                    self.request_redraw();
                    return;
                }
                self.quit(el);
            }
            WindowEvent::Focused(f) => {
                self.focused = f;
                if f {
                    // you're here now: the active tab's attention is served
                    if let Some(t) = self.tabs.get_mut(self.active) {
                        t.attention = false;
                    }
                    self.request_redraw();
                }
            }
            WindowEvent::DroppedFile(path) => {
                // a dropped file lands at the prompt as a shell-ready path
                let arg = term::dropped_path_arg(&path);
                if let Some(s) = self.session_mut() {
                    s.write(arg.as_bytes());
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::ScaleFactorChanged { .. } => {
                // HiDPI: the window moved to a monitor with another scale.
                // Rebuild cell metrics off-thread; the current renderer keeps
                // painting until RendererReady swaps it in and re-snaps the grid.
                self.rebuild_renderer();
            }
            WindowEvent::Resized(px) => {
                if px.width == 0 || px.height == 0 {
                    return;
                }
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(px.width, px.height);
                }
                self.relayout_active();
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = position;
                if let Some(i) = self.drag_divider {
                    self.drag_divider_to(i, position);
                    return;
                }
                if self.mouse_left_down {
                    // the drag stays in the pane where it started
                    if let Some((pi, row, col, side)) = self.pane_cell_at(position) {
                        if pi == self.sel_pane {
                            if let Some(s) = self.session() {
                                let mut t = s.term.lock().unwrap();
                                let off = t.grid().display_offset() as i32;
                                let point = Point::new(Line(row as i32 - off), Column(col));
                                if let Some(sel) = t.selection.as_mut() {
                                    sel.update(point, side);
                                }
                            }
                            self.request_redraw();
                        }
                    }
                } else if let Some(w) = &self.window {
                    // hovering a divider shows the resize cursor
                    use winit::window::CursorIcon;
                    let icon = if self.divider_at(position).is_some() {
                        match self.tabs.get(self.active).map(|t| t.orientation) {
                            Some(Orientation::Column) => CursorIcon::NsResize,
                            _ => CursorIcon::EwResize,
                        }
                    } else {
                        CursorIcon::Default
                    };
                    w.set_cursor(icon);
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => {
                    // the shortcuts editor swallows clicks: a row starts a
                    // capture, outside the card closes + saves
                    if self.shortcuts.is_some() {
                        self.shortcuts_click(self.cursor_pos.x, self.cursor_pos.y);
                        return;
                    }
                    // the settings panel swallows clicks: apply the control
                    // hit, or a click outside the card closes + saves
                    if self.settings.is_some() {
                        self.settings_click(self.cursor_pos.x, self.cursor_pos.y);
                        return;
                    }
                    // the settings gear button in the tab bar opens settings
                    // (a click — layout-proof, unlike a comma shortcut)
                    if let (Some(r), Some(w)) = (self.renderer.as_ref(), self.window.as_ref()) {
                        let pw = w.inner_size().width as usize;
                        if r.settings_button_hit(pw, self.cursor_pos.x, self.cursor_pos.y) {
                            self.settings = Some(0);
                            self.request_redraw();
                            return;
                        }
                    }
                    // the cockpit swallows clicks: pick a row or dismiss
                    if self.cockpit {
                        let hit = self.cockpit_row_hit(self.cursor_pos);
                        self.cockpit = false;
                        if let Some(row) = hit {
                            self.switch_tab(row);
                        }
                        self.request_redraw();
                        return;
                    }
                    // grabbing a divider starts a resize drag
                    if let Some(i) = self.divider_at(self.cursor_pos) {
                        self.drag_divider = Some(i);
                        return;
                    }
                    // a click in the tab bar switches tabs
                    if let Some(idx) = self.tab_at(self.cursor_pos) {
                        self.switch_tab(idx);
                        return;
                    }
                    if let Some((pi, row, col, side)) = self.pane_cell_at(self.cursor_pos) {
                        // clicking a pane focuses it
                        if let Some(t) = self.tabs.get_mut(self.active) {
                            if t.focused != pi {
                                t.focused = pi;
                                self.update_title();
                            }
                        }
                        self.sel_pane = pi;
                        // 1 click = simple drag anchor, 2 = word, 3 = line
                        let n = self.clicks.click(Instant::now(), (row, col));
                        let ty = match n {
                            2 => SelectionType::Semantic,
                            3 => SelectionType::Lines,
                            _ => SelectionType::Simple,
                        };
                        if let Some(s) = self.session() {
                            let mut t = s.term.lock().unwrap();
                            let off = t.grid().display_offset() as i32;
                            let point = Point::new(Line(row as i32 - off), Column(col));
                            t.selection = Some(Selection::new(ty, point, side));
                        }
                        self.mouse_left_down = true;
                        if n >= 2 && self.config.copy_on_select_on() {
                            // word/line selections are complete on click
                            self.copy_selection(false);
                        }
                        self.request_redraw();
                    }
                }
                ElementState::Released => {
                    if self.drag_divider.take().is_some() {
                        return; // a resize drag copies nothing
                    }
                    self.mouse_left_down = false;
                    // EasyTer convention: auto-copy the selection on release
                    // (the settings panel can turn this off; explicit
                    // Ctrl+C / Ctrl+Shift+C always copy)
                    if self.config.copy_on_select_on() {
                        self.copy_selection(false);
                    }
                }
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let cell_h = self.renderer.as_ref().map_or(20.0, |r| r.cell_h);
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 3.0,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / cell_h,
                };
                // accumulate: precision touchpads send many sub-line deltas
                self.wheel_accum += lines;
                let n = self.wheel_accum as i32;
                if n == 0 {
                    return;
                }
                self.wheel_accum -= n as f32;
                // Ctrl+wheel = live font zoom (EasyTer's favorite)
                if self.modifiers.control_key() {
                    self.font_delta = (self.font_delta + n as f32).clamp(-7.0, 25.0);
                    self.rebuild_renderer();
                    return;
                }
                let mode = self.term_mode();
                if mode.contains(TermMode::ALT_SCREEN) {
                    // a full-screen TUI owns scrolling. EasyTer's rule: SGR
                    // mouse only if the app asked for mouse reporting, else
                    // arrow keys (honoring DECCKM) — never injected garbage.
                    let seq = if mode.intersects(TermMode::MOUSE_MODE) {
                        let (row, col) = self.pointer_cell_1based();
                        let btn = if n > 0 { 64 } else { 65 };
                        format!("\x1b[<{btn};{col};{row}M")
                    } else {
                        let ch = if n > 0 { 'A' } else { 'B' };
                        if mode.contains(TermMode::APP_CURSOR) {
                            format!("\x1bO{ch}")
                        } else {
                            format!("\x1b[{ch}")
                        }
                    };
                    let payload = seq.repeat(n.unsigned_abs() as usize);
                    if let Some(s) = self.session_mut() {
                        s.write(payload.as_bytes());
                    }
                } else {
                    self.scroll_lines(n);
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed =>
            {
                let m = self.modifiers;
                let (ctrl, shift, alt) = (m.control_key(), m.shift_key(), m.alt_key());
                crate::prof::mark(&format!(
                    "key logical={:?} physical={:?} text={:?} ctrl={ctrl} shift={shift}",
                    event.logical_key, event.physical_key, event.text
                ));
                // a pending close owns the keyboard: Enter closes, Esc keeps
                if let Some(target) = self.pending_close {
                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) => {
                            self.pending_close = None;
                            match target {
                                CloseTarget::Pane(ti, pi) => self.remove_pane(ti, pi, el),
                                CloseTarget::Window => self.quit(el),
                            }
                        }
                        Key::Named(NamedKey::Escape) => self.pending_close = None,
                        _ => {}
                    }
                    self.request_redraw();
                    return;
                }
                // a pending paste owns the keyboard: Enter sends, Esc drops
                if self.pending_paste.is_some() {
                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) => {
                            if let Some(txt) = self.pending_paste.take() {
                                self.send_paste(&txt);
                            }
                        }
                        Key::Named(NamedKey::Escape) => self.pending_paste = None,
                        _ => {}
                    }
                    self.request_redraw();
                    return;
                }
                // the shortcuts editor owns the keyboard while open
                if self.shortcuts.is_some() {
                    self.shortcuts_input(&event, el);
                    return;
                }
                // the settings panel owns the keyboard while open
                if self.settings.is_some() {
                    self.settings_input(&event);
                    return;
                }
                // the cockpit owns the keyboard while open
                if self.cockpit {
                    self.cockpit_input(&event);
                    return;
                }
                // the search bar owns the keyboard while open
                if self.search.is_some() {
                    self.search_input(&event, shift);
                    return;
                }
                let key = &event.logical_key;
                // ---- rebindable shortcuts (the keymap; Ctrl+, ⌨ edits it).
                // EasyTer's TUI rule, generalized: a PLAIN ctrl+letter chord
                // belongs to a full-screen TUI when one is up — pass it on.
                if let Some(chord) =
                    keybinds::chord_from(event.physical_key, ctrl, shift, alt)
                {
                    if let Some(action) = keybinds::lookup(&self.keymap, chord) {
                        let tui_owns = chord.is_plain_ctrl_letter()
                            && self.term_mode().contains(TermMode::ALT_SCREEN);
                        if !tui_owns {
                            self.perform_action(action, el);
                            return;
                        }
                    }
                }
                // Alt+arrows move focus between panes (EasyTer's binding)
                if alt && !ctrl {
                    let step = match key {
                        Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::ArrowUp) => {
                            Some(-1i32)
                        }
                        Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::ArrowDown) => {
                            Some(1)
                        }
                        _ => None,
                    };
                    if let Some(d) = step {
                        if let Some(t) = self.tabs.get_mut(self.active) {
                            if t.panes.len() > 1 {
                                let n = t.panes.len() as i32;
                                t.focused =
                                    ((t.focused as i32 + d + n) % n) as usize;
                                self.update_title();
                                self.request_redraw();
                                return;
                            }
                        }
                    }
                } else if ctrl {
                    // Ctrl+Tab / Ctrl+Shift+Tab cycle tabs (shift handled here
                    // because winit reports Tab+shift with the shift modifier)
                    if matches!(key, Key::Named(NamedKey::Tab)) && !self.tabs.is_empty() {
                        let n = self.tabs.len();
                        let next = if shift {
                            (self.active + n - 1) % n
                        } else {
                            (self.active + 1) % n
                        };
                        self.switch_tab(next);
                        return;
                    }
                    // Ctrl+0 resets the font zoom (physical digit key:
                    // works on the Arabic layout too)
                    {
                        use winit::keyboard::{KeyCode, PhysicalKey};
                        if !shift
                            && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Digit0))
                        {
                            self.font_delta = 0.0;
                            self.rebuild_renderer();
                            return;
                        }
                    }
                    // Ctrl+C copies when a selection exists (Windows
                    // convention); otherwise falls through to \x03
                    if let Some(l) = key_letter(&event) {
                        if !shift && l == 'c' && self.has_selection() {
                            self.copy_selection(true);
                            self.request_redraw();
                            return;
                        }
                    }
                }
                // Shift+PageUp/PageDown page through the scrollback
                if shift && !ctrl {
                    let rows = self.size.rows as i32;
                    match key {
                        Key::Named(NamedKey::PageUp) => {
                            self.scroll_lines(rows);
                            return;
                        }
                        Key::Named(NamedKey::PageDown) => {
                            self.scroll_lines(-rows);
                            return;
                        }
                        _ => {}
                    }
                }
                let text = event.text.as_ref().map(|t| t.as_str());
                // Ctrl+letter resolves by PHYSICAL key so the control byte is
                // right on any layout (Arabic: Ctrl+C must interrupt even
                // though the C key's logical char is "ؤ")
                let bytes = if ctrl && !alt {
                    key_letter(&event).map(|l| vec![l as u8 - b'a' + 1])
                } else {
                    None
                }
                .or_else(|| keys::encode(key, text, shift, alt, ctrl));
                if let Some(bytes) = bytes {
                    // typing snaps to the bottom and clears any selection;
                    // it also restarts the blink window, cursor visible
                    self.last_input = Instant::now();
                    if let Some(s) = self.session_mut() {
                        {
                            let mut t = s.term.lock().unwrap();
                            t.selection = None;
                            t.scroll_display(Scroll::Bottom);
                        }
                        s.write(&bytes);
                    }
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
}

fn main() {
    prof::init();
    prof::mark("main start");
    use winit::platform::windows::EventLoopBuilderExtWindows;
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .with_msg_hook(|msg| {
            use windows_sys::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY};
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
    let cfg = config::load();
    let keymap = keybinds::effective_map(&cfg);
    let mut app = App {
        proxy,
        window: None,
        gpu: None,
        renderer: None,
        tabs: Vec::new(),
        active: 0,
        next_id: 0,
        size: TermSize { cols: 110, rows: 32 },
        modifiers: ModifiersState::default(),
        first_frame: false,
        first_output: false,
        last_frame: Vec::new(),
        cursor_pos: PhysicalPosition::new(0.0, 0.0),
        mouse_left_down: false,
        sel_pane: 0,
        drag_divider: None,
        cockpit: false,
        cockpit_sel: 0,
        pending_paste: None,
        pending_close: None,
        last_input: Instant::now(),
        blink_drawn_on: true,
        settings: None,
        focused: true,
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
    };
    event_loop.run_app(&mut app).expect("run event loop");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
        let feed = |p: &mut Processor, t: &mut Term<VoidListener>,
                    m: &mut term::SessionMeta, c: &mut Vec<u8>, d: &[u8]| {
            term::process_chunk(p, t, m, c, d);
        };
        feed(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;A\x07> ok\r\n");
        feed(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;D;0\x07");
        feed(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;A\x07> bad\r\n");
        feed(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;D;1\x07");
        assert_eq!(meta.marks.len(), 2);
        assert_eq!(meta.marks[0].exit, Some(0));
        assert_eq!(meta.marks[1].exit, Some(1));
        assert!(meta.marks[1].abs > meta.marks[0].abs);
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
        };
        let json = serde_json::to_string(&s).unwrap();
        let back = parse_state(&json).unwrap();
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
        let m1 = find_match(&mut t, "needle", &None, Direction::Left)
            .expect("first match");
        assert_eq!(m1.start().line.0, 2);
        // Enter again: steps up to "needle one" (row 0)
        let m2 = find_match(&mut t, "needle", &Some(m1.clone()), Direction::Left)
            .expect("second match");
        assert_eq!(m2.start().line.0, 0);
        // Enter once more: wraps around back to row 2
        let m3 = find_match(&mut t, "needle", &Some(m2.clone()), Direction::Left)
            .expect("wrapped match");
        assert_eq!(m3.start().line.0, 2);
        // Shift+Enter from the first hit walks back down
        let m4 = find_match(&mut t, "needle", &Some(m2), Direction::Right)
            .expect("forward match");
        assert_eq!(m4.start().line.0, 2);
        // no hits -> None, and a regex-special query is treated literally
        assert!(find_match(&mut t, "absent", &None, Direction::Left).is_none());
        assert!(find_match(&mut t, "needle (", &None, Direction::Left).is_none());
    }

    #[test]
    fn step_point_walks_and_clamps() {
        let (cols, top, bottom) = (10, -100, 31);
        let p = Point::new(Line(0), Column(9));
        assert_eq!(step_point(p, cols, top, bottom, Direction::Right),
                   Point::new(Line(1), Column(0)));
        let q = Point::new(Line(0), Column(0));
        assert_eq!(step_point(q, cols, top, bottom, Direction::Left),
                   Point::new(Line(-1), Column(9)));
        // clamped at the extremes
        let end = Point::new(Line(bottom), Column(9));
        assert_eq!(step_point(end, cols, top, bottom, Direction::Right), end);
        let start = Point::new(Line(top), Column(0));
        assert_eq!(step_point(start, cols, top, bottom, Direction::Left), start);
    }
}
