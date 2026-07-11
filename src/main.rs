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

use std::num::NonZeroU32;
use std::rc::Rc;

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use term::{Session, TermSize};

#[derive(Debug)]
pub enum UserEvent {
    /// New PTY output was parsed: repaint.
    Wakeup,
    /// The emulator wants to answer the child (DSR/DA replies TUIs block on).
    PtyWrite(String),
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
        let (Some(window), Some(surface), Some(renderer), Some(session)) = (
            self.window.as_ref(),
            self.surface.as_mut(),
            self.renderer.as_mut(),
            self.session.as_ref(),
        ) else {
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
        {
            let t = session.term.lock().unwrap();
            renderer.draw(&mut buffer, px.width as usize, px.height as usize, &t);
        }
        let _ = buffer.present();
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
        self.renderer = Some(render::Renderer::new(window.scale_factor() as f32));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let mut surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        let px = window.inner_size();
        if let (Some(w), Some(h)) = (NonZeroU32::new(px.width), NonZeroU32::new(px.height)) {
            let _ = surface.resize(w, h);
        }
        self.surface = Some(surface);
        self.size = self.grid_for(px);
        self.session =
            Some(Session::spawn(self.size, self.proxy.clone()).expect("spawn shell"));
        self.window = Some(window);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wakeup => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::PtyWrite(text) => {
                if let Some(s) = self.session.as_mut() {
                    s.write(text.as_bytes());
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
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App {
        proxy,
        window: None,
        surface: None,
        renderer: None,
        session: None,
        size: TermSize { cols: 110, rows: 32 },
        modifiers: ModifiersState::default(),
    };
    event_loop.run_app(&mut app).expect("run event loop");
}
