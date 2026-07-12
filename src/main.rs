//! Bayan (بيان) — an Arabic-first, agent-ready terminal.
//!
//! Milestone M1: one window, one PowerShell over ConPTY, correct Arabic
//! shaping and BiDi from day one. Architecture mirrors EasyTer (Python/Qt),
//! whose regression suites serve as the behavioral specification.

// release builds are pure GUI apps: no companion console window
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod keys;
mod render;
mod term;

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

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use term::{Session, TermSize};

pub enum UserEvent {
    /// New PTY output was parsed: repaint.
    Wakeup,
    /// The emulator wants to answer the child (DSR/DA replies TUIs block on).
    PtyWrite(String),
    /// The font system finished loading on its background thread.
    RendererReady(Box<render::Renderer>),
    /// The shell exited or the reader thread died.
    Exit,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    renderer: Option<render::Renderer>,
    session: Option<Session>,
    size: TermSize,
    modifiers: ModifiersState,
    first_frame: bool,
    first_output: bool,
}

impl App {
    fn grid_for(&self, px: PhysicalSize<u32>) -> TermSize {
        let r = self.renderer.as_ref().expect("renderer initialized");
        TermSize {
            cols: ((px.width as f32 / r.cell_w) as usize).max(20),
            rows: ((px.height as f32 / r.cell_h) as usize).max(5),
        }
    }

    fn redraw(&mut self) {
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
        match (self.renderer.as_mut(), self.session.as_ref()) {
            (Some(renderer), Some(session)) => {
                let t = session.term.lock().unwrap();
                renderer.draw(&mut buffer, px.width as usize, px.height as usize, &t);
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
        // once cell metrics exist (EasyTer starts 110x32 the same way)
        self.session =
            Some(Session::spawn(self.size, self.proxy.clone()).expect("spawn shell"));
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
            UserEvent::Wakeup => {
                if !self.first_output {
                    self.first_output = true;
                    crate::prof::mark("first pty output");
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::PtyWrite(text) => {
                if let Some(s) = self.session.as_mut() {
                    s.write(text.as_bytes());
                }
            }
            UserEvent::RendererReady(r) => {
                crate::prof::mark("renderer ready");
                self.renderer = Some(*r);
                if let Some(w) = &self.window {
                    // now that cell metrics exist, snap the grid to the window
                    let g = self.grid_for(w.inner_size());
                    if g != self.size {
                        self.size = g;
                        if let Some(s) = self.session.as_mut() {
                            s.resize(g);
                        }
                    }
                    w.request_redraw();
                }
            }
            UserEvent::Exit => el.exit(),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
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
                        self.size = g;
                        if let Some(s) = self.session.as_mut() {
                            s.resize(g);
                        }
                    }
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed =>
            {
                let m = self.modifiers;
                let text = event.text.as_ref().map(|t| t.as_str());
                if let Some(bytes) = keys::encode(
                    &event.logical_key,
                    text,
                    m.shift_key(),
                    m.alt_key(),
                    m.control_key(),
                ) {
                    if let Some(s) = self.session.as_mut() {
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
        session: None,
        size: TermSize { cols: 110, rows: 32 },
        modifiers: ModifiersState::default(),
        first_frame: false,
        first_output: false,
    };
    event_loop.run_app(&mut app).expect("run event loop");
}
