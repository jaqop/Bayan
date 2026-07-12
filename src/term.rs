//! PTY session + terminal state: ConPTY via portable-pty, VT emulation via
//! alacritty_terminal. Mirrors EasyTer's proven architecture: a reader thread
//! feeds the emulator behind a mutex and nudges the UI thread to repaint.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

pub const DEFAULT_SHELL: &str = "powershell.exe";
pub const SCROLLBACK: usize = 10_000;

/// Pull the oh-my-posh theme path out of a PowerShell profile line like
///   oh-my-posh init pwsh --config "C:\...\night-owl.omp.json" | Invoke-Expression
pub fn parse_posh_config(profile_text: &str) -> Option<String> {
    for line in profile_text.lines() {
        let l = line.trim();
        if l.starts_with('#') || !l.contains("oh-my-posh") || !l.contains("--config") {
            continue;
        }
        let rest = l[l.find("--config")? + "--config".len()..].trim_start();
        let path = if let Some(q) = rest.strip_prefix('"') {
            q.split('"').next()
        } else if let Some(q) = rest.strip_prefix('\'') {
            q.split('\'').next()
        } else {
            rest.split_whitespace().next()
        };
        if let Some(p) = path {
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// The user's oh-my-posh theme: POSH_THEME env first, else auto-detected
/// from the PowerShell profile. Lets Bayan skip the profile (-NoProfile is
/// the single biggest avoidable startup cost — the EasyTer lesson) while
/// keeping the prompt the user actually uses.
fn detect_posh_theme() -> Option<String> {
    if let Ok(t) = std::env::var("POSH_THEME") {
        if !t.is_empty() && std::path::Path::new(&t).exists() {
            return Some(t);
        }
    }
    let home = std::env::var("USERPROFILE").ok()?;
    for rel in [
        r"Documents\PowerShell\Microsoft.PowerShell_profile.ps1",
        r"Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1",
    ] {
        let p = std::path::Path::new(&home).join(rel);
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Some(cfg) = parse_posh_config(&text) {
                if std::path::Path::new(&cfg).exists() {
                    return Some(cfg);
                }
            }
        }
    }
    None
}

/// Grid dimensions handed to alacritty_terminal.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Routes emulator events to the winit loop. PtyWrite matters most: TUIs
/// (Claude Code among them) send ESC[6n and block waiting for the cursor
/// position reply — the exact lesson EasyTer's ReportingScreen taught us.
#[derive(Clone)]
pub struct EventProxy(pub EventLoopProxy<UserEvent>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mapped = match event {
            Event::PtyWrite(text) => UserEvent::PtyWrite(text),
            Event::Wakeup => UserEvent::Wakeup,
            Event::Exit => UserEvent::Exit,
            _ => return,
        };
        let _ = self.0.send_event(mapped);
    }
}

pub struct Session {
    pub term: Arc<Mutex<Term<EventProxy>>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn Child + Send + Sync>,
}

impl Session {
    pub fn spawn(
        size: TermSize,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows: size.rows as u16,
            cols: size.cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(DEFAULT_SHELL);
        // never inherit the launcher's directory (often system32): start home
        if let Ok(home) = std::env::var("USERPROFILE") {
            cmd.cwd(home);
        }
        // -NoProfile: the profile is seconds of avoidable startup (it re-runs
        // oh-my-posh, chcp, modules). Self-provide what it gave us: UTF-8
        // (Arabic!) and the user's own prompt theme, auto-detected.
        cmd.args(["-NoProfile", "-NoExit", "-Command"]);
        let mut setup = String::from(
            "$OutputEncoding=[Console]::InputEncoding=[Console]::OutputEncoding=\
             [Text.UTF8Encoding]::new();",
        );
        if let Some(theme) = detect_posh_theme() {
            setup.push_str(&format!(
                " $ErrorActionPreference='SilentlyContinue'; \
                 & {{oh-my-posh init powershell --config '{}' | Invoke-Expression}} 2>$null; \
                 $ErrorActionPreference='Continue';",
                theme.replace('\'', "''")
            ));
        }
        cmd.arg(setup);
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        let term = Arc::new(Mutex::new(Term::new(
            config,
            &size,
            EventProxy(proxy.clone()),
        )));

        let term2 = Arc::clone(&term);
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::new();
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        {
                            let mut t = term2.lock().unwrap();
                            for &b in &buf[..n] {
                                parser.advance(&mut *t, b);
                            }
                        }
                        if proxy.send_event(UserEvent::Wakeup).is_err() {
                            break; // the event loop is gone
                        }
                    }
                }
            }
            let _ = proxy.send_event(UserEvent::Exit);
        });

        Ok(Self {
            term,
            writer,
            master: pair.master,
            _child: child,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&mut self, size: TermSize) {
        let _ = self.master.resize(PtySize {
            rows: size.rows as u16,
            cols: size.cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.term.lock().unwrap().resize(size);
    }
}

/// Prepare clipboard text for the PTY: newlines become carriage returns, and
/// under bracketed paste the payload is wrapped — with any embedded end
/// marker stripped so pasted content can't terminate paste mode early and
/// smuggle the remainder in as executed input (EasyTer's paste-injection fix).
pub fn normalize_paste(text: &str, bracketed: bool) -> String {
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\x1b[200~{}\x1b[201~", body.replace("\x1b[201~", ""))
    } else {
        body
    }
}

/// Escape a literal so RegexSearch treats it verbatim (search is literal
/// text, not regex, from the user's point of view).
pub fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if r"\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_normalizes_newlines_and_blocks_injection() {
        assert_eq!(normalize_paste("a\r\nb\nc", false), "a\rb\rc");
        // bracketed: wrapped, and an embedded end marker cannot break out
        assert_eq!(
            normalize_paste("safe\x1b[201~evil\n", true),
            "\x1b[200~safeevil\r\x1b[201~"
        );
    }

    #[test]
    fn regex_escape_makes_literals() {
        assert_eq!(regex_escape("a.b*c"), r"a\.b\*c");
        assert_eq!(regex_escape("plain"), "plain");
        assert_eq!(regex_escape("x(1)[2]{3}"), r"x\(1\)\[2\]\{3\}");
    }

    #[test]
    fn posh_config_parses_common_profile_lines() {
        assert_eq!(
            parse_posh_config(
                r#"oh-my-posh init pwsh --config "C:\Users\Admin\.poshthemes\night-owl.omp.json" | Invoke-Expression"#
            ),
            Some(r"C:\Users\Admin\.poshthemes\night-owl.omp.json".to_string())
        );
        assert_eq!(
            parse_posh_config("oh-my-posh init powershell --config 'D:\\t\\x.omp.json' | iex"),
            Some(r"D:\t\x.omp.json".to_string())
        );
        assert_eq!(
            parse_posh_config("oh-my-posh init pwsh --config C:\\plain\\path.omp.json | iex"),
            Some(r"C:\plain\path.omp.json".to_string())
        );
        // commented-out lines and unrelated profiles yield nothing
        assert_eq!(
            parse_posh_config("# oh-my-posh init pwsh --config x.json\nSet-Alias g git"),
            None
        );
    }
}
