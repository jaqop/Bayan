//! Reload `config.json` when it changes on disk.
//!
//! The settings panel used to be the only writer. An editor saving the same
//! file — including one that writes a temp file and renames it into place —
//! is applied without restarting. A burst of writes waits until things have
//! been quiet for [`Debounce::QUIET`], then one parse runs. A parse error
//! keeps the last good config. A write the panel itself just made is not
//! loaded again, so saving cannot loop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::Watcher;

use crate::config::{self, UserConfig};

/// Quiet period after the last filesystem event before the file is read.
#[derive(Default)]
pub struct Debounce {
    deadline: Option<Instant>,
}

impl Debounce {
    pub const QUIET: Duration = Duration::from_millis(200);

    /// Another write arrived. The quiet period starts over.
    pub fn push(&mut self, now: Instant) {
        self.deadline = Some(now + Self::QUIET);
    }

    /// True once, when `now` is at or past the end of the quiet period.
    pub fn take(&mut self, now: Instant) -> bool {
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.deadline = None;
                true
            }
            _ => false,
        }
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }
}

/// What the running window can change immediately.
///
/// `font` rebuilds cell metrics; the renderer swap lays the panes out again
/// and resizes each PTY. `padding` and `hide_bar` resize with the metrics
/// already on screen. Colors rebuild the renderer. The other flags update
/// state the frame or the next keypress already reads. The flags are
/// independent side effects, not a mode.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate side effect of a reload"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyPlan {
    pub colors: bool,
    pub font: bool,
    pub padding: bool,
    pub opacity: bool,
    pub cursor: bool,
    pub keybinds: bool,
    pub language: bool,
    pub hide_bar: bool,
}

/// A change that does not affect the panes already open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredSetting {
    /// History length is fixed when the pane is created.
    Scrollback,
    /// New tabs spawn this program. Live panes keep theirs.
    Shell,
    /// Read once, when the window starts.
    RestoreTabs,
    /// Plugins are loaded once before the first frame.
    Plugins,
    /// The pane's terminal is built with the flag. New panes pick it up.
    KittyProtocol,
}

impl DeferredSetting {
    fn phrase(self) -> &'static str {
        match self {
            Self::Scrollback => {
                crate::lang::tr("سجل التمرير للوحات الجديدة", "scrollback for new panes")
            }
            Self::Shell => crate::lang::tr("الصدفة للتبويبات الجديدة", "shell for new tabs"),
            Self::RestoreTabs => crate::lang::tr(
                "استعادة التبويبات عند التشغيل التالي",
                "restore tabs on next launch",
            ),
            Self::Plugins => crate::lang::tr("الإضافات بعد إعادة التشغيل", "plugins after restart"),
            Self::KittyProtocol => crate::lang::tr(
                "بروتوكول لوحة kitty للوحات الجديدة",
                "kitty keyboard protocol for new panes",
            ),
        }
    }
}

/// One quiet period produced a decision.
#[derive(Clone, Debug)]
pub enum ReloadMsg {
    Applied {
        config: Box<UserConfig>,
        plan: ApplyPlan,
        deferred: Vec<DeferredSetting>,
    },
    /// The file is missing, unreadable, or not JSON. `config` in the watcher
    /// is still the previous good one.
    Rejected { error: String },
}

/// Notice for settings that wait for a new pane, a new tab, or a restart.
pub fn deferred_notice(items: &[DeferredSetting]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let sep = if crate::lang::rtl() { "، " } else { ", " };
    let list = items
        .iter()
        .map(|item| item.phrase())
        .collect::<Vec<_>>()
        .join(sep);
    Some(format!(
        "{} {list}",
        crate::lang::tr("يُطبَّق لاحقاً:", "Takes effect later:")
    ))
}

/// Notice for a file that could not be parsed. The running config is unchanged.
pub fn parse_error_notice(error: &str) -> String {
    format!(
        "{} — {error}",
        crate::lang::tr(
            "تعذّر قراءة الإعدادات؛ بقيت آخر إعدادات سليمة",
            "Could not read settings; keeping the last good ones",
        )
    )
}

/// A path that is `config.json`, or a temp name an editor parks beside it
/// (`config.json.tmp`, `config.json.12.1.tmp`, `config.json~`).
pub fn path_names_config(path: &Path, config: &Path) -> bool {
    let (Some(want), Some(got)) = (config.file_name(), path.file_name()) else {
        return false;
    };
    if got == want {
        return true;
    }
    let got = got.to_string_lossy();
    let want = want.to_string_lossy();
    let Some(rest) = got.strip_prefix(want.as_ref()) else {
        return false;
    };
    rest.starts_with(['.', '~'])
}

/// True when a notify event might be an editor saving `config.json`.
/// Access events are ignored so our own read does not schedule another one.
/// A rescan means the backend lost events; the file is read either way.
pub fn event_hits_config(event: &notify::Event, config: &Path) -> bool {
    if event.kind.is_access() {
        return false;
    }
    if event.need_rescan() {
        return true;
    }
    event
        .paths
        .iter()
        .any(|path| path_names_config(path, config))
}

/// The watched directory itself went away. The watch has to be armed again
/// when the directory is recreated; events for files inside it are not this.
pub fn directory_watch_lost(event: &notify::Event, dir: &Path) -> bool {
    event.kind.is_remove() && event.paths.iter().any(|path| path == dir)
}

fn diff(prev: &UserConfig, next: &UserConfig) -> (ApplyPlan, Vec<DeferredSetting>) {
    // Behavioral fields with no flag (copy on select, bell, osc52, the editor,
    // kitty's base-layout fallback, …) are live because the caller swaps the
    // whole config in and the next use reads them.
    let plan = ApplyPlan {
        colors: prev.theme != next.theme
            || prev.bg != next.bg
            || prev.fg != next.fg
            || prev.palette != next.palette,
        font: prev.font_family != next.font_family
            || prev.arabic_font != next.arabic_font
            || prev.font_size != next.font_size
            || prev.ligatures != next.ligatures,
        padding: prev.padding_px() != next.padding_px(),
        opacity: (prev.opacity_level() - next.opacity_level()).abs() > f32::EPSILON,
        cursor: prev.cursor() != next.cursor() || prev.cursor_blink_on() != next.cursor_blink_on(),
        keybinds: prev.keybinds != next.keybinds,
        language: prev.lang() != next.lang(),
        hide_bar: prev.hide_single_tab_on() != next.hide_single_tab_on(),
    };

    let mut deferred = Vec::new();
    if prev.scrollback_lines() != next.scrollback_lines() {
        deferred.push(DeferredSetting::Scrollback);
    }
    if prev.shell_program() != next.shell_program() {
        deferred.push(DeferredSetting::Shell);
    }
    if prev.restore_tabs.unwrap_or(false) != next.restore_tabs.unwrap_or(false) {
        deferred.push(DeferredSetting::RestoreTabs);
    }
    if prev.disabled_plugins() != next.disabled_plugins() {
        deferred.push(DeferredSetting::Plugins);
    }
    if prev.kitty_protocol_on() != next.kitty_protocol_on() {
        deferred.push(DeferredSetting::KittyProtocol);
    }
    (plan, deferred)
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    Missing,
    Bytes(u64),
}

struct WatchState {
    last_good: UserConfig,
    seen: Option<Seen>,
    /// Hash of the bytes the panel is writing. One read of that exact
    /// payload is ignored.
    suppress: Option<u64>,
    /// Set for the gap between arming `suppress` and the rename finishing,
    /// so a debounce that fires in that gap cannot reload the old file.
    write_in_progress: bool,
    had_error: bool,
}

impl WatchState {
    fn new(config: UserConfig) -> Self {
        Self {
            last_good: config,
            seen: None,
            suppress: None,
            write_in_progress: false,
            had_error: false,
        }
    }

    /// Remember the bytes already loaded at startup. The same bytes must
    /// not produce a second apply, and a debug hook layered on top of them
    /// must not be wiped by the watcher's first event.
    fn prime(&mut self, bytes: Option<&[u8]>) {
        match bytes {
            None => self.seen = Some(Seen::Missing),
            Some(bytes) => {
                self.seen = Some(Seen::Bytes(hash_bytes(bytes)));
                self.had_error = config::parse_bytes(bytes).is_err();
            }
        }
    }

    fn note_self_write(&mut self, bytes: &[u8]) {
        self.suppress = Some(hash_bytes(bytes));
        self.write_in_progress = true;
    }

    fn finish_self_write(&mut self, wrote: bool) {
        self.write_in_progress = false;
        if !wrote {
            self.suppress = None;
        }
    }

    /// The running session has the config on screen. Until this, a parsed
    /// file is only a proposal: `last_good` stays at the last one applied.
    fn commit_applied(&mut self, config: &UserConfig) {
        self.last_good = config.clone();
        self.had_error = false;
    }

    fn ingest_file(&mut self, path: &Path) -> Option<ReloadMsg> {
        self.ingest_result(std::fs::read(path))
    }

    fn ingest_result(&mut self, read: std::io::Result<Vec<u8>>) -> Option<ReloadMsg> {
        if self.write_in_progress {
            return None;
        }
        match read {
            Ok(bytes) => self.ingest(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.note_missing(),
            Err(error) => self.note_unreadable(&error),
        }
    }

    fn note_missing(&mut self) -> Option<ReloadMsg> {
        if self.seen == Some(Seen::Missing) {
            return None;
        }
        self.seen = Some(Seen::Missing);
        // Deleting the file is the same as never having one: defaults, and
        // the panel may save them back. It is not a parse error, so it must
        // not block that save. `last_good` moves only when the session
        // applies this.
        let defaults = UserConfig::default();
        let (plan, deferred) = diff(&self.last_good, &defaults);
        let was_error = self.had_error;
        self.had_error = false;
        if !was_error && defaults == self.last_good {
            return None;
        }
        Some(ReloadMsg::Applied {
            config: Box::new(defaults),
            plan,
            deferred,
        })
    }

    fn note_unreadable(&mut self, error: &std::io::Error) -> Option<ReloadMsg> {
        let hash = hash_bytes(error.to_string().as_bytes());
        if self.seen == Some(Seen::Bytes(hash)) {
            return None;
        }
        self.seen = Some(Seen::Bytes(hash));
        self.had_error = true;
        Some(ReloadMsg::Rejected {
            error: error.to_string(),
        })
    }

    fn ingest(&mut self, bytes: &[u8]) -> Option<ReloadMsg> {
        if self.write_in_progress {
            return None;
        }
        let hash = hash_bytes(bytes);
        if self.suppress == Some(hash) {
            self.suppress = None;
            self.seen = Some(Seen::Bytes(hash));
            if let Ok(cfg) = config::parse_bytes(bytes) {
                self.last_good = cfg;
                self.had_error = false;
            }
            return None;
        }
        // A different payload means the panel's write is not what is on disk.
        self.suppress = None;
        if self.seen == Some(Seen::Bytes(hash)) {
            return None;
        }
        self.seen = Some(Seen::Bytes(hash));
        match config::parse_bytes(bytes) {
            Err(error) => {
                self.had_error = true;
                Some(ReloadMsg::Rejected {
                    error: error.to_string(),
                })
            }
            Ok(cfg) => {
                let (plan, deferred) = diff(&self.last_good, &cfg);
                let was_error = self.had_error;
                self.had_error = false;
                if !was_error && cfg == self.last_good {
                    return None;
                }
                // Leave last_good. The session commits it when the new
                // config is actually on screen, so a renderer build that
                // is still in flight cannot strand a newer file.
                Some(ReloadMsg::Applied {
                    config: Box::new(cfg),
                    plan,
                    deferred,
                })
            }
        }
    }
}

fn lock(state: &Mutex<WatchState>) -> MutexGuard<'_, WatchState> {
    state.lock().unwrap_or_else(|poison| poison.into_inner())
}

enum Wake {
    Fs(notify::Result<notify::Event>),
    Stop,
}

/// Owns the watch thread. Dropping it stops and joins the thread.
pub struct ConfigWatcher {
    state: Arc<Mutex<WatchState>>,
    stop: Sender<Wake>,
    thread: Option<JoinHandle<()>>,
    /// Set once `watch` has succeeded. The rename-replace test waits on it;
    /// the running terminal does not.
    #[cfg_attr(not(test), allow(dead_code))]
    ready: Arc<AtomicBool>,
}

impl ConfigWatcher {
    /// Arm suppression and mark a write in progress. Call `finish_self_write`
    /// after the rename returns.
    pub fn note_self_write(&self, bytes: &[u8]) {
        lock(&self.state).note_self_write(bytes);
    }

    pub fn finish_self_write(&self, wrote: bool) {
        lock(&self.state).finish_self_write(wrote);
    }

    /// The config is on screen (the renderer caught up, or nothing visual
    /// had to be rebuilt). Further reads diff against this.
    pub fn commit_applied(&self, config: &UserConfig) {
        lock(&self.state).commit_applied(config);
    }

    /// Block until the directory watch is installed, or the thread has exited.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.ready.load(Ordering::Relaxed) {
                return true;
            }
            if self
                .thread
                .as_ref()
                .is_none_or(|thread| thread.is_finished())
            {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.ready.load(Ordering::Relaxed)
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        let _ = self.stop.send(Wake::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Watch `path`'s directory. `baseline` is the config the window is already
/// running. `baseline_bytes` is what was on disk at startup (`None` if the
/// file was absent); those bytes are not applied a second time.
pub fn spawn(
    path: PathBuf,
    baseline: UserConfig,
    baseline_bytes: Option<&[u8]>,
    on_reload: impl Fn(ReloadMsg) + Send + 'static,
) -> ConfigWatcher {
    let mut primed = WatchState::new(baseline);
    primed.prime(baseline_bytes);
    let state = Arc::new(Mutex::new(primed));
    let ready = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let callback = tx.clone();
    let thread_state = Arc::clone(&state);
    let thread_ready = Arc::clone(&ready);
    let thread = std::thread::Builder::new()
        .name("bayan-config".to_string())
        .spawn(move || watch_loop(path, thread_state, thread_ready, callback, rx, on_reload))
        .map_err(|error| eprintln!("bayan: config watch thread failed: {error}"))
        .ok();
    ConfigWatcher {
        state,
        stop: tx,
        thread,
        ready,
    }
}

/// One renderer build at a time. A reload that arrives while a build is
/// running is remembered (`config_gen`) and started when that build
/// finishes, so the installed renderer ends on the latest config rather
/// than the one captured at the start of the in-flight build.
#[derive(Clone, Debug)]
pub struct RendererSlot {
    building: bool,
    config_gen: u64,
    inflight_gen: u64,
    inflight_delta: f32,
    wanted: UserConfig,
    inflight: UserConfig,
    /// Config the installed renderer was built from. The session does not
    /// read it; the reload-during-build test does.
    #[cfg_attr(not(test), allow(dead_code))]
    rendered: Option<UserConfig>,
}

impl RendererSlot {
    pub fn new(config: UserConfig) -> Self {
        Self {
            building: false,
            config_gen: 0,
            inflight_gen: 0,
            inflight_delta: 0.0,
            wanted: config.clone(),
            inflight: config,
            rendered: None,
        }
    }

    /// `config` is what the session wants on screen. A change bumps the
    /// generation so a build that already started is known to be stale.
    pub fn note_wanted(&mut self, config: &UserConfig) {
        if &self.wanted != config {
            self.wanted = config.clone();
            self.config_gen = self.config_gen.wrapping_add(1);
        }
    }

    /// Start a build of the wanted config, or return false when one is
    /// already running. The caller keeps painting the previous renderer.
    pub fn try_begin(&mut self, font_delta: f32) -> bool {
        if self.building {
            return false;
        }
        self.building = true;
        self.inflight_gen = self.config_gen;
        self.inflight_delta = font_delta;
        self.inflight = self.wanted.clone();
        true
    }

    /// The config the in-flight build captured. Valid after `try_begin`.
    pub fn snapshot(&self) -> &UserConfig {
        &self.inflight
    }

    /// The build finished. True when the installed renderer is behind the
    /// latest config or the zoom moved, so the caller starts another build.
    pub fn finish(&mut self, font_delta: f32) -> bool {
        self.building = false;
        self.rendered = Some(self.inflight.clone());
        self.config_gen != self.inflight_gen
            || (font_delta - self.inflight_delta).abs() > f32::EPSILON
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rendered(&self) -> Option<&UserConfig> {
        self.rendered.as_ref()
    }
}

fn watch_loop(
    path: PathBuf,
    state: Arc<Mutex<WatchState>>,
    ready: Arc<AtomicBool>,
    tx: Sender<Wake>,
    rx: Receiver<Wake>,
    on_reload: impl Fn(ReloadMsg),
) {
    let mut watcher = match notify::recommended_watcher(move |result| {
        let _ = tx.send(Wake::Fs(result));
    }) {
        Ok(watcher) => watcher,
        Err(error) => {
            eprintln!("bayan: config watch unavailable: {error}");
            return;
        }
    };
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if !wait_until_watched(&mut watcher, &dir, &rx) {
        return;
    }
    loop {
        if let Some(msg) = lock(&state).ingest_file(&path) {
            on_reload(msg);
        }
        ready.store(true, Ordering::Relaxed);
        let mut debounce = Debounce::default();
        let lost = loop {
            let wake = match debounce.remaining(Instant::now()) {
                None => match rx.recv() {
                    Ok(wake) => wake,
                    Err(_) => break false,
                },
                Some(wait) => match rx.recv_timeout(wait) {
                    Ok(wake) => wake,
                    Err(RecvTimeoutError::Timeout) => {
                        if debounce.take(Instant::now()) {
                            if let Some(msg) = lock(&state).ingest_file(&path) {
                                on_reload(msg);
                            }
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break false,
                },
            };
            match wake {
                Wake::Stop => break false,
                Wake::Fs(Err(_)) => break true,
                Wake::Fs(Ok(event)) if directory_watch_lost(&event, &dir) => break true,
                Wake::Fs(Ok(event)) if event_hits_config(&event, &path) => {
                    debounce.push(Instant::now());
                }
                Wake::Fs(_) => {}
            }
        };
        ready.store(false, Ordering::Relaxed);
        if !lost {
            return;
        }
        // The directory was removed or the backend dropped the watch.
        // Wait until it exists again, then read whatever config.json is there.
        let _ = watcher.unwatch(&dir);
        if !wait_until_watched(&mut watcher, &dir, &rx) {
            return;
        }
    }
}

#[cfg(test)]
pub fn reload_missing(baseline: UserConfig) -> Option<ReloadMsg> {
    let bytes = crate::config::encode(&baseline).unwrap();
    let mut state = WatchState::new(baseline.clone());
    state.prime(Some(&bytes));
    state.commit_applied(&baseline);
    state.ingest_result(Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "deleted",
    )))
}

/// Read `next` as a new config.json over a session already running `baseline`
/// (the bytes that were on disk at startup). Tests use this so a reload is
/// the same parse the watcher thread runs.
#[cfg(test)]
pub fn reload_from_bytes(
    baseline: UserConfig,
    baseline_bytes: &[u8],
    next: &[u8],
) -> Option<ReloadMsg> {
    let mut state = WatchState::new(baseline);
    state.prime(Some(baseline_bytes));
    state.ingest(next)
}

fn wait_until_watched(watcher: &mut impl notify::Watcher, dir: &Path, rx: &Receiver<Wake>) -> bool {
    loop {
        match watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
            Ok(()) => return true,
            Err(_) => match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Wake::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
                Ok(Wake::Fs(_)) | Err(RecvTimeoutError::Timeout) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CursorStyle;
    use crate::keybinds::{self, Action};
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "bayan-config-watch-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn applied(msg: ReloadMsg) -> (UserConfig, ApplyPlan, Vec<DeferredSetting>) {
        match msg {
            ReloadMsg::Applied {
                config,
                plan,
                deferred,
            } => (*config, plan, deferred),
            ReloadMsg::Rejected { error } => panic!("expected an applied config, got {error}"),
        }
    }

    #[test]
    fn file_change_reloads_and_applies_live_values() {
        let mut state = WatchState::new(UserConfig::default());
        state.prime(Some(b"{}"));
        let text = r##"{
            "font_family": "Cascadia Code",
            "arabic_font": "Amiri",
            "font_size": 22.0,
            "ligatures": false,
            "padding": 16,
            "cursor_style": "bar",
            "cursor_blink": false,
            "theme": "Bayan",
            "bg": "#112233",
            "fg": "#eeeeee",
            "opacity": 0.8,
            "language": "en",
            "hide_single_tab": true,
            "copy_on_select": false,
            "keybinds": { "copy": "ctrl+shift+c" },
            "scrollback": 20000,
            "shell": "pwsh.exe",
            "restore_tabs": true,
            "disabled_plugins": ["demo"],
            "keyboard": { "kitty_protocol": true, "kitty_base_layout_fallback": false }
        }"##;
        let (config, plan, deferred) = applied(state.ingest(text.as_bytes()).unwrap());
        assert_eq!(config.font_family.as_deref(), Some("Cascadia Code"));
        assert_eq!(config.arabic_font.as_deref(), Some("Amiri"));
        assert_eq!(config.font_size, Some(22.0));
        assert_eq!(config.ligatures, Some(false));
        assert_eq!(config.padding_px(), 16);
        assert_eq!(config.cursor(), CursorStyle::Bar);
        assert!(!config.cursor_blink_on());
        assert_eq!(config.theme.as_deref(), Some("Bayan"));
        assert_eq!(config.bg.as_deref(), Some("#112233"));
        assert_eq!(config.fg.as_deref(), Some("#eeeeee"));
        assert_eq!(config.opacity_level(), 0.8);
        assert_eq!(config.lang(), crate::lang::Lang::En);
        assert!(config.hide_single_tab_on());
        assert!(!config.copy_on_select_on());
        // A font change rebuilds metrics; the renderer swap lays the panes
        // out again and resizes every PTY. Padding resizes even without that.
        assert!(plan.font, "font family and size resize the pty");
        assert!(plan.padding, "padding resizes the pty");
        assert!(plan.colors);
        assert!(plan.cursor);
        assert!(plan.opacity);
        assert!(plan.keybinds);
        assert!(plan.language);
        assert!(plan.hide_bar);
        let map = keybinds::effective_map(&config);
        let copy = map
            .iter()
            .find(|(action, _)| *action == Action::Copy)
            .unwrap();
        assert_eq!(copy.1, keybinds::Chord::parse("ctrl+shift+c").unwrap());
        assert!(
            !config.osc52_copy_enabled(),
            "a reload must not turn OSC 52 on"
        );
        assert!(!config.kitty_base_layout_fallback_on());
        assert_eq!(
            deferred,
            vec![
                DeferredSetting::Scrollback,
                DeferredSetting::Shell,
                DeferredSetting::RestoreTabs,
                DeferredSetting::Plugins,
                DeferredSetting::KittyProtocol,
            ]
        );
        crate::lang::set(crate::lang::Lang::En);
        let notice = deferred_notice(&deferred).unwrap();
        assert!(notice.starts_with("Takes effect later:"));
        assert!(notice.contains("scrollback for new panes"));
        assert!(notice.contains("shell for new tabs"));
        assert!(!notice.contains("font"));
        crate::lang::set(crate::lang::Lang::Ar);
        let ar = deferred_notice(&deferred).unwrap();
        assert!(ar.starts_with("يُطبَّق لاحقاً:"));
        assert!(ar.contains("سجل التمرير"));
    }

    #[test]
    fn invalid_file_keeps_the_last_good_config_and_the_error() {
        let mut state = WatchState::new(UserConfig::default());
        state.prime(Some(b"{}"));
        let good = br##"{"font_size":22.0,"bg":"#112233","cursor_style":"bar","osc52":"copy"}"##;
        let (config, _, _) = applied(state.ingest(good).unwrap());
        state.commit_applied(&config);
        assert_eq!(config.font_size, Some(22.0));
        assert!(config.osc52_copy_enabled());

        let ReloadMsg::Rejected { error } = state.ingest(b"{oops").unwrap() else {
            panic!("broken json must be an error, not a new config");
        };
        assert!(!error.is_empty());
        let notice = parse_error_notice(&error);
        assert!(notice.contains(&error));
        assert_eq!(state.last_good.font_size, Some(22.0));
        assert_eq!(state.last_good.bg.as_deref(), Some("#112233"));
        assert_eq!(state.last_good.cursor(), CursorStyle::Bar);
        assert!(
            state.last_good.osc52_copy_enabled(),
            "a parse error must not reset osc52 or anything else"
        );
        // The same broken bytes do not reset, and do not raise a second error.
        assert!(state.ingest(b"{oops").is_none());

        let (config, plan, _) = applied(state.ingest(br##"{"font_size":22.0,"bg":"#112233","cursor_style":"underline","osc52":"copy"}"##).unwrap());
        state.commit_applied(&config);
        assert_eq!(config.cursor(), CursorStyle::Underline);
        assert!(plan.cursor);
        assert_eq!(config.font_size, Some(22.0));
        assert!(config.osc52_copy_enabled());
        assert_eq!(state.last_good.cursor(), CursorStyle::Underline);
    }

    /// A config the corrupt-file tests start from. Several fields are set so
    /// a parser that applied the keys it saw before the file ended would
    /// show up as a change.
    fn rich_settings() -> UserConfig {
        UserConfig {
            font_size: Some(22.0),
            arabic_font: Some("Amiri".into()),
            cursor_style: Some("bar".into()),
            bg: Some("#112233".into()),
            osc52: Some("copy".into()),
            padding: Some(12),
            bidi: Some(crate::config::BidiConfig { honor_bdsm: false }),
            ..UserConfig::default()
        }
    }

    fn primed_rich() -> WatchState {
        let cfg = rich_settings();
        let bytes = config::encode(&cfg).unwrap();
        let mut state = WatchState::new(cfg);
        state.prime(Some(&bytes));
        state
    }

    /// `bytes` is not a config. The running settings stay byte-for-byte the
    /// same, including fields the broken text had already started to name.
    fn assert_corrupt_keeps_everything(state: &mut WatchState, bytes: &[u8]) {
        let before = state.last_good.clone();
        match state.ingest(bytes) {
            Some(ReloadMsg::Rejected { error }) => assert!(
                !error.is_empty(),
                "a bad file must report why it was refused"
            ),
            Some(ReloadMsg::Applied { config, .. }) => {
                panic!("corrupt file applied {config:?}")
            }
            None => panic!("corrupt file produced no error"),
        }
        assert_eq!(
            state.last_good, before,
            "a bad file must not apply any field"
        );
        assert!(
            state.ingest(bytes).is_none(),
            "the same bad bytes must not be applied on the next read"
        );
        assert_eq!(state.last_good, before);
    }

    #[test]
    fn invalid_json_keeps_every_current_setting() {
        let mut state = primed_rich();
        assert_corrupt_keeps_everything(&mut state, b"{oops");
        assert_corrupt_keeps_everything(&mut state, b"not json at all");
        // A UTF-8 break in the middle of an otherwise plausible object.
        assert_corrupt_keeps_everything(&mut state, b"{\"font_size\":99.0,\xff}");
        assert_eq!(state.last_good.font_size, Some(22.0));
        assert_eq!(state.last_good.arabic_font.as_deref(), Some("Amiri"));
        assert!(state.last_good.osc52_copy_enabled());
        assert!(!state.last_good.honor_bdsm());
    }

    #[test]
    fn empty_config_file_keeps_every_current_setting() {
        let mut state = primed_rich();
        assert_corrupt_keeps_everything(&mut state, b"");
        assert_corrupt_keeps_everything(&mut state, b"\n\n");
        assert_corrupt_keeps_everything(&mut state, b"\xef\xbb\xbf");
        assert_eq!(state.last_good, rich_settings());
    }

    #[test]
    fn truncated_config_write_keeps_every_current_setting() {
        let mut state = primed_rich();
        // font_size and cursor_style are on disk, but the object never
        // closes. Those keys must not land.
        assert_corrupt_keeps_everything(
            &mut state,
            br#"{"font_size":99.0,"cursor_style":"underline","arabic_font":"Kawkab"#,
        );
        assert_corrupt_keeps_everything(&mut state, b"{\n  \"font_size\": 99.0,\n");
        assert_corrupt_keeps_everything(&mut state, b"{\"padding\":");
        assert_eq!(state.last_good.font_size, Some(22.0));
        assert_eq!(state.last_good.cursor(), CursorStyle::Bar);
        assert_eq!(state.last_good.arabic_font.as_deref(), Some("Amiri"));
        assert_eq!(state.last_good.padding_px(), 12);
        assert_eq!(state.last_good.bg.as_deref(), Some("#112233"));
        assert!(state.last_good.osc52_copy_enabled());
        assert!(!state.last_good.honor_bdsm());
    }

    #[test]
    fn debounce_coalesces_rapid_writes() {
        let mut debounce = Debounce::default();
        let t0 = Instant::now();
        let writes_ms = [0u64, 30, 70, 120, 180];
        let mut reloads = 0;
        let mut next_write = 0;
        for step in 0..80 {
            let now = t0 + Duration::from_millis(step * 10);
            while next_write < writes_ms.len() && writes_ms[next_write] <= step * 10 {
                debounce.push(now);
                next_write += 1;
            }
            if debounce.take(now) {
                reloads += 1;
            }
        }
        assert_eq!(
            reloads, 1,
            "five writes inside the quiet window are one reload"
        );
        let later = t0 + Duration::from_millis(800);
        debounce.push(later);
        assert!(!debounce.take(later + Duration::from_millis(199)));
        assert!(debounce.take(later + Debounce::QUIET));
        assert!(!debounce.take(later + Duration::from_millis(400)));
    }

    #[test]
    fn rename_replace_save_is_detected() {
        let config = PathBuf::from("/home/user/.bayan/config.json");
        let tmp = PathBuf::from("/home/user/.bayan/config.json.42.1.tmp");
        let session = PathBuf::from("/home/user/.bayan/session.json");
        assert!(path_names_config(&config, &config));
        assert!(path_names_config(&tmp, &config));
        assert!(path_names_config(
            &PathBuf::from("/home/user/.bayan/config.json~"),
            &config
        ));
        assert!(!path_names_config(&session, &config));
        let renamed = notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Name(notify::event::RenameMode::Both),
        ))
        .add_path(tmp)
        .add_path(config.clone());
        assert!(event_hits_config(&renamed, &config));
        let access = notify::Event::new(notify::EventKind::Access(notify::event::AccessKind::Read))
            .add_path(config.clone());
        assert!(!event_hits_config(&access, &config));

        // The real watcher: write a temp file and rename it onto config.json,
        // which is how the panel and most editors save.
        let fixture = Fixture::new();
        let path = fixture.0.join("config.json");
        std::fs::write(&path, b"{}\n").unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = spawn(
            path.clone(),
            UserConfig::default(),
            Some(b"{}\n"),
            move |msg| {
                let _ = tx.send(msg);
            },
        );
        assert!(
            watcher.wait_ready(Duration::from_secs(2)),
            "the directory watch did not start"
        );
        let editing = fixture.0.join("config.json.editing");
        std::fs::write(
            &editing,
            br#"{"font_size":21.0,"cursor_style":"underline","padding":8}"#,
        )
        .unwrap();
        std::fs::rename(&editing, &path).unwrap();
        let msg = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("rename-replace must be noticed");
        let (config, plan, deferred) = applied(msg);
        assert_eq!(config.font_size, Some(21.0));
        assert_eq!(config.cursor(), CursorStyle::Underline);
        assert_eq!(config.padding_px(), 8);
        assert!(plan.font);
        assert!(plan.cursor);
        assert!(plan.padding);
        assert!(deferred.is_empty());
        assert!(!config.osc52_copy_enabled());
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "one rename is one reload"
        );
    }

    #[test]
    fn self_write_from_the_panel_does_not_loop() {
        let mut state = WatchState::new(UserConfig::default());
        state.prime(Some(b"{}"));
        let cfg = UserConfig {
            font_size: Some(20.0),
            padding: Some(12),
            ..UserConfig::default()
        };
        let bytes = config::encode(&cfg).unwrap();

        // Without the suppress mark, the watch would apply and a caller that
        // saved again would write the file a second time.
        let (loaded, plan, _) = applied(state.ingest(&bytes).unwrap());
        assert_eq!(loaded.font_size, Some(20.0));
        assert!(plan.font);
        assert!(plan.padding);

        let mut again = loaded.clone();
        again.cursor_style = Some("bar".into());
        let panel = config::encode(&again).unwrap();
        let mut writes = 0;
        state.note_self_write(&panel);
        writes += 1;
        // A read that lands while the rename is still in progress must not
        // apply, and must not consume the suppress mark.
        assert!(state.ingest(&panel).is_none());
        state.finish_self_write(true);
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 5, "panel save reloaded itself");
            match state.ingest(&panel) {
                None => break,
                Some(ReloadMsg::Applied { config, .. }) => {
                    let echoed = config::encode(&config).unwrap();
                    state.note_self_write(&echoed);
                    state.finish_self_write(true);
                    writes += 1;
                }
                Some(ReloadMsg::Rejected { error }) => {
                    panic!("the panel's own json was rejected: {error}")
                }
            }
        }
        assert_eq!(writes, 1);
        assert_eq!(state.last_good.font_size, Some(20.0));
        assert_eq!(state.last_good.cursor(), CursorStyle::Bar);

        // An edit that is not the panel's bytes still applies once.
        let mut external = state.last_good.clone();
        external.opacity = Some(0.7);
        let external_bytes = config::encode(&external).unwrap();
        let (loaded, plan, _) = applied(state.ingest(&external_bytes).unwrap());
        assert_eq!(loaded.opacity_level(), 0.7);
        assert!(plan.opacity);
        assert!(!loaded.osc52_copy_enabled());
    }

    #[test]
    fn osc52_stays_off_when_a_reload_does_not_say_copy() {
        let mut state = WatchState::new(UserConfig::default());
        state.prime(Some(b"{}"));
        for raw in [
            br#"{"font_size":16.0}"#.as_slice(),
            br#"{"osc52":"on"}"#,
            br#"{"osc52":"paste"}"#,
            br#"{"osc52":"off"}"#,
        ] {
            let (config, _, _) = applied(state.ingest(raw).unwrap());
            assert!(
                !config.osc52_copy_enabled(),
                "reload of {} enabled OSC 52",
                std::str::from_utf8(raw).unwrap()
            );
        }
        let (config, _, _) = applied(state.ingest(br#"{"osc52":"copy"}"#).unwrap());
        assert!(config.osc52_copy_enabled());
    }

    #[test]
    fn reload_during_a_build_applies_the_latest_config() {
        let mut slot = RendererSlot::new(UserConfig::default());
        assert!(slot.try_begin(0.0), "the startup build starts");
        let during_startup = UserConfig {
            font_size: Some(18.0),
            bg: Some("#010203".into()),
            ..UserConfig::default()
        };
        slot.note_wanted(&during_startup);
        assert!(
            !slot.try_begin(0.0),
            "a reload during the initial build must not start a second one"
        );
        assert!(
            slot.finish(0.0),
            "the startup renderer is behind the file that arrived"
        );
        assert!(slot.try_begin(0.0));
        assert!(!slot.finish(0.0));
        assert_eq!(slot.rendered().unwrap(), &during_startup);

        let second = UserConfig {
            font_size: Some(22.0),
            ..during_startup.clone()
        };
        let latest = UserConfig {
            font_size: Some(30.0),
            fg: Some("#eeeecc".into()),
            ..second.clone()
        };
        slot.note_wanted(&second);
        assert!(slot.try_begin(0.0));
        slot.note_wanted(&latest);
        assert!(
            !slot.try_begin(0.0),
            "the second reload waits for the build already running"
        );
        assert!(slot.finish(0.0));
        assert!(slot.try_begin(0.0));
        assert!(!slot.finish(0.0));
        assert_eq!(
            slot.rendered().unwrap(),
            &latest,
            "the installed renderer is the latest file, not the one in flight"
        );

        // last_good stays on the previous applied config until that renderer
        // is actually installed.
        let mut state = WatchState::new(UserConfig::default());
        state.prime(Some(b"{}"));
        let bytes = config::encode(&latest).unwrap();
        let (parsed, plan, _) = applied(state.ingest(&bytes).unwrap());
        assert!(plan.font);
        assert!(plan.colors);
        assert_eq!(
            state.last_good.font_size, None,
            "parsing the file must not pretend the renderer already has it"
        );
        state.commit_applied(slot.rendered().unwrap());
        assert_eq!(state.last_good, parsed);
        assert_eq!(state.last_good, latest);
    }

    #[test]
    fn removing_the_config_directory_is_a_lost_watch() {
        let dir = std::path::PathBuf::from("/home/user/.bayan");
        let file = dir.join("config.json");
        let removed =
            notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::Folder))
                .add_path(dir.clone());
        assert!(directory_watch_lost(&removed, &dir));
        let file_removed =
            notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::File))
                .add_path(file);
        assert!(!directory_watch_lost(&file_removed, &dir));
    }
}
