//! Bayan (بيان) — an Arabic-first, agent-ready terminal.
//!
//! Milestone M1: one window, one PowerShell over ConPTY, correct Arabic
//! shaping and BiDi from day one. Architecture mirrors EasyTer (Python/Qt),
//! whose regression suites serve as the behavioral specification.

// release builds are pure GUI apps: no companion console window
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bidi;
mod keys;
mod render;
mod term;

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

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::TermMode;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
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
    /// Tab `id`'s shell reported its working directory (OSC 9;9).
    Cwd(u64, String),
    /// Tab `id`'s shell exited.
    Exit(u64),
}

/// One terminal tab.
struct Tab {
    id: u64,
    session: Session,
    /// cwd basename (falls back to a plain name)
    title: String,
    /// when this tab last produced output — feeds the busy dot
    last_output: Option<Instant>,
}

/// What survives a restart: each tab's cwd + the active index.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedState {
    tabs: Vec<Option<String>>,
    active: usize,
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
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    renderer: Option<render::Renderer>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    size: TermSize,
    modifiers: ModifiersState,
    first_frame: bool,
    first_output: bool,
    cursor_pos: PhysicalPosition<f64>,
    mouse_left_down: bool,
    clicks: ClickTracker,
    wheel_accum: f32,
    search: Option<SearchState>,
    clipboard: Option<arboard::Clipboard>,
    /// Claude mode follows (alt screen + claude command) automatically;
    /// F2 switches to manual and back (EasyTer's toggle semantics).
    auto_follow: bool,
    claude_manual: bool,
}

impl App {
    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn session(&self) -> Option<&Session> {
        self.tabs.get(self.active).map(|t| &t.session)
    }

    fn session_mut(&mut self) -> Option<&mut Session> {
        self.tabs.get_mut(self.active).map(|t| &mut t.session)
    }

    fn spawn_tab(&mut self, cwd: Option<String>) {
        let id = self.next_id;
        self.next_id += 1;
        match Session::spawn(self.size, self.proxy.clone(), id, cwd.as_deref()) {
            Ok(session) => {
                let title = cwd
                    .as_deref()
                    .and_then(|c| c.rsplit(['\\', '/']).next().map(str::to_string))
                    .unwrap_or_else(|| "بيان".to_string());
                self.tabs.push(Tab { id, session, title, last_output: None });
                self.active = self.tabs.len() - 1;
            }
            Err(e) => eprintln!("bayan: spawn tab failed: {e}"),
        }
    }

    fn switch_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() || idx == self.active {
            return;
        }
        self.active = idx;
        self.search = None; // the bar belongs to the tab that opened it
        self.update_title();
        self.request_redraw();
    }

    fn close_tab(&mut self, idx: usize, el: &ActiveEventLoop) {
        if idx >= self.tabs.len() {
            return;
        }
        let mut tab = self.tabs.remove(idx);
        tab.session.kill();
        if self.tabs.is_empty() {
            self.save_state();
            el.exit();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        self.update_title();
        self.request_redraw();
    }

    fn update_title(&self) {
        if let (Some(w), Some(t)) = (&self.window, self.tabs.get(self.active)) {
            w.set_title(&format!("Bayan — بيان · {}", t.title));
        }
    }

    fn save_state(&self) {
        let Some(path) = state_path() else { return };
        let state = SavedState {
            tabs: self
                .tabs
                .iter()
                .map(|t| t.session.meta.lock().unwrap().cwd.clone())
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
            .and_then(|s| serde_json::from_str(&s).ok());
        match saved {
            Some(state) if !state.tabs.is_empty() => {
                for cwd in state.tabs {
                    self.spawn_tab(cwd);
                }
                self.active = state.active.min(self.tabs.len().saturating_sub(1));
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

    /// Pixel position -> (viewport row, col, cell side) for selection.
    /// None inside the tab bar (that region belongs to tab switching).
    fn cell_at(&self, pos: PhysicalPosition<f64>) -> Option<(usize, usize, Side)> {
        let r = self.renderer.as_ref()?;
        if self.size.cols == 0 || self.size.rows == 0 {
            return None;
        }
        let y = pos.y - r.tab_bar_h() as f64;
        if y < 0.0 {
            return None;
        }
        let colf = pos.x / r.cell_w as f64;
        let col = (colf.floor().max(0.0) as usize).min(self.size.cols - 1);
        let row = ((y / r.cell_h as f64).floor().max(0.0) as usize)
            .min(self.size.rows - 1);
        let side = if colf - col as f64 <= 0.5 { Side::Left } else { Side::Right };
        Some((row, col, side))
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
        let (cw, ch, bar) = self
            .renderer
            .as_ref()
            .map_or((9.0, 20.0, 0.0), |r| (r.cell_w, r.cell_h, r.tab_bar_h()));
        let col = (self.cursor_pos.x / cw as f64).floor().max(0.0) as usize + 1;
        let row = (((self.cursor_pos.y - bar as f64) / ch as f64).floor().max(0.0)) as usize + 1;
        (row, col)
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
        let bracketed = self.term_mode().contains(TermMode::BRACKETED_PASTE);
        let body = term::normalize_paste(&txt, bracketed);
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

    fn grid_for(&self, px: PhysicalSize<u32>) -> TermSize {
        let r = self.renderer.as_ref().expect("renderer initialized");
        TermSize {
            cols: ((px.width as f32 / r.cell_w) as usize).max(20),
            rows: (((px.height as f32 - r.tab_bar_h()) / r.cell_h) as usize).max(5),
        }
    }

    /// The grid changed (resize, new metrics): every tab's PTY follows.
    fn resize_all(&mut self, size: TermSize) {
        self.size = size;
        for tab in &mut self.tabs {
            tab.session.resize(size);
        }
    }

    fn redraw(&mut self) {
        let claude = self.claude_active();
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut())
        else {
            return;
        };
        let px = window.inner_size();
        if px.width == 0 || px.height == 0 {
            return;
        }
        let mut buffer = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        let tab_infos: Vec<render::TabInfo> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| render::TabInfo {
                title: t.title.clone(),
                busy: i != self.active
                    && t.last_output
                        .is_some_and(|ts| ts.elapsed() < std::time::Duration::from_secs(2)),
                active: i == self.active,
            })
            .collect();
        match (self.renderer.as_mut(), self.tabs.get(self.active)) {
            (Some(renderer), Some(tab)) => {
                let overlay = render::Overlay {
                    search_query: self.search.as_ref().map(|s| s.query.as_str()),
                    search_match: self.search.as_ref().and_then(|s| s.hl.as_ref()),
                    claude,
                    tabs: &tab_infos,
                };
                let t = tab.session.term.lock().unwrap();
                renderer.draw(&mut buffer, px.width as usize, px.height as usize, &t, &overlay);
            }
            // renderer still warming up on its thread: dark frame, instantly
            _ => buffer.fill(render::bg_packed()),
        }
        let _ = buffer.present();
        if !self.first_frame {
            self.first_frame = true;
            crate::prof::mark("first frame presented");
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Bayan — بيان")
            .with_inner_size(LogicalSize::new(1100.0, 700.0));
        let window = Rc::new(el.create_window(attrs).expect("create window"));
        crate::prof::mark("window created");
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let mut surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        let px = window.inner_size();
        if let (Some(w), Some(h)) = (NonZeroU32::new(px.width), NonZeroU32::new(px.height)) {
            let _ = surface.resize(w, h);
        }
        self.surface = Some(surface);
        self.window = Some(window.clone());
        // first frame NOW: a dark window on screen beats a frozen launcher.
        // The shell and the font system warm up behind it, in parallel.
        self.redraw();
        // the PTY + conhost + PowerShell chain is the slowest dependency:
        // start it immediately with a default grid; the exact grid follows
        // once cell metrics exist (EasyTer starts 110x32 the same way).
        // Restores yesterday's tabs (each in its saved cwd) or opens one.
        self.restore_state();
        crate::prof::mark("session spawned");
        // FontSystem::new scans every installed font — the documented
        // cosmic-text startup cost (pop-os/cosmic-text#247): keep it off
        // the UI thread and swap the renderer in when it's ready
        let proxy = self.proxy.clone();
        let scale = window.scale_factor() as f32;
        std::thread::spawn(move || {
            let r = render::Renderer::new(scale);
            let _ = proxy.send_event(UserEvent::RendererReady(Box::new(r)));
        });
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wakeup(id) => {
                if !self.first_output {
                    self.first_output = true;
                    crate::prof::mark("first pty output");
                }
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
                    t.last_output = Some(Instant::now());
                }
                self.request_redraw();
            }
            UserEvent::PtyWrite(id, text) => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
                    t.session.write(text.as_bytes());
                }
            }
            UserEvent::ClipboardSet(text) => self.set_clipboard(text),
            UserEvent::Cwd(id, cwd) => {
                let base = cwd.rsplit(['\\', '/']).next().unwrap_or(&cwd).to_string();
                let is_active = self
                    .tabs
                    .iter()
                    .position(|t| t.id == id)
                    .is_some_and(|i| i == self.active);
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
                    t.title = base;
                }
                if is_active {
                    self.update_title();
                }
                self.request_redraw(); // tab titles live in the frame
            }
            UserEvent::RendererReady(r) => {
                crate::prof::mark("renderer ready");
                self.renderer = Some(*r);
                if let Some(px) = self.window.as_ref().map(|w| w.inner_size()) {
                    // now that cell metrics exist, snap the grid to the window
                    let g = self.grid_for(px);
                    if g != self.size {
                        self.resize_all(g);
                    }
                }
                self.request_redraw();
            }
            UserEvent::Exit(id) => {
                // that tab's shell ended: close it; last one closes Bayan
                if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
                    self.close_tab(i, el);
                }
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // busy dots decay after ~2s of quiet: keep repainting on a slow
        // heartbeat only while any background tab is (or just was) active
        let any_busy = self.tabs.iter().enumerate().any(|(i, t)| {
            i != self.active
                && t.last_output
                    .is_some_and(|ts| ts.elapsed() < std::time::Duration::from_secs(3))
        });
        if any_busy {
            el.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + std::time::Duration::from_millis(600),
            ));
            self.request_redraw();
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.save_state(); // tabs + cwds greet you tomorrow
                el.exit();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // HiDPI: the window moved to a monitor with another scale.
                // Rebuild cell metrics off-thread; the current renderer keeps
                // painting until RendererReady swaps it in and re-snaps the grid.
                let proxy = self.proxy.clone();
                let scale = scale_factor as f32;
                std::thread::spawn(move || {
                    let r = render::Renderer::new(scale);
                    let _ = proxy.send_event(UserEvent::RendererReady(Box::new(r)));
                });
            }
            WindowEvent::Resized(px) => {
                if px.width == 0 || px.height == 0 {
                    return;
                }
                if let Some(surface) = self.surface.as_mut() {
                    if let (Some(w), Some(h)) =
                        (NonZeroU32::new(px.width), NonZeroU32::new(px.height))
                    {
                        let _ = surface.resize(w, h);
                    }
                }
                if self.renderer.is_some() {
                    let g = self.grid_for(px);
                    if g != self.size {
                        self.resize_all(g);
                    }
                }
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = position;
                if self.mouse_left_down {
                    if let Some((row, col, side)) = self.cell_at(position) {
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
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => {
                    // a click in the tab bar switches tabs
                    if let Some(idx) = self.tab_at(self.cursor_pos) {
                        self.switch_tab(idx);
                        return;
                    }
                    if let Some((row, col, side)) = self.cell_at(self.cursor_pos) {
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
                        if n >= 2 {
                            // word/line selections are complete on click
                            self.copy_selection(false);
                        }
                        self.request_redraw();
                    }
                }
                ElementState::Released => {
                    self.mouse_left_down = false;
                    // EasyTer convention: auto-copy the selection on release
                    self.copy_selection(false);
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
                    "key {:?} text={:?} ctrl={ctrl} shift={shift}",
                    event.logical_key, event.text
                ));
                // the search bar owns the keyboard while open
                if self.search.is_some() {
                    self.search_input(&event, shift);
                    return;
                }
                let key = &event.logical_key;
                // F2: Claude mode manual toggle (auto -> manual flip -> auto)
                if matches!(key, Key::Named(NamedKey::F2)) {
                    if self.auto_follow {
                        self.claude_manual = !self.claude_active();
                        self.auto_follow = false;
                    } else {
                        self.auto_follow = true;
                    }
                    self.request_redraw();
                    return;
                }
                // ---- app-level shortcuts (EasyTer's rules) ----
                if ctrl && shift {
                    match key_letter(&event) {
                        Some('c') => {
                            self.copy_selection(false);
                            return;
                        }
                        Some('v') => {
                            self.paste();
                            return;
                        }
                        _ => {}
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
                    if let Some(l) = key_letter(&event) {
                        if shift && l == 'w' {
                            // Ctrl+Shift+W closes the tab (EasyTer's binding)
                            self.close_tab(self.active, el);
                            return;
                        }
                        // Ctrl+C copies when a selection exists (Windows
                        // convention); otherwise falls through to \x03
                        if !shift && l == 'c' && self.has_selection() {
                            self.copy_selection(true);
                            self.request_redraw();
                            return;
                        }
                        // plain Ctrl+T/V/F belong to a full-screen TUI
                        if !shift && (l == 'v' || l == 'f' || l == 't') {
                            let alt_screen =
                                self.term_mode().contains(TermMode::ALT_SCREEN);
                            if l == 't' && !alt_screen {
                                // new tab inherits the active tab's directory
                                let cwd = self
                                    .session()
                                    .and_then(|s| s.meta.lock().unwrap().cwd.clone());
                                self.spawn_tab(cwd);
                                self.update_title();
                                self.request_redraw();
                                return;
                            }
                            if l == 'v' && !alt_screen {
                                self.paste();
                                return;
                            }
                            if l == 'f' && !alt_screen {
                                self.search = Some(SearchState::default());
                                self.request_redraw();
                                return;
                            }
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
                    // typing snaps to the bottom and clears any selection
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
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    prof::mark("event loop built");
    let proxy = event_loop.create_proxy();
    let mut app = App {
        proxy,
        window: None,
        surface: None,
        renderer: None,
        tabs: Vec::new(),
        active: 0,
        next_id: 0,
        size: TermSize { cols: 110, rows: 32 },
        modifiers: ModifiersState::default(),
        first_frame: false,
        first_output: false,
        cursor_pos: PhysicalPosition::new(0.0, 0.0),
        mouse_left_down: false,
        clicks: ClickTracker::new(),
        wheel_accum: 0.0,
        search: None,
        clipboard: None,
        auto_follow: true,
        claude_manual: false,
    };
    event_loop.run_app(&mut app).expect("run event loop");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn saved_state_round_trips() {
        let s = SavedState {
            tabs: vec![Some(r"C:\Users\Admin\Bayan".into()), None],
            active: 1,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: SavedState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tabs.len(), 2);
        assert_eq!(back.tabs[0].as_deref(), Some(r"C:\Users\Admin\Bayan"));
        assert_eq!(back.active, 1);
        // a corrupt file must not crash restore (it falls back to one tab)
        assert!(serde_json::from_str::<SavedState>("{broken").is_err());
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
            for b in line.bytes() {
                p.advance(&mut t, b);
            }
            p.advance(&mut t, b'\r');
            p.advance(&mut t, b'\n');
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
