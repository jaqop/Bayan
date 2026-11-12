//! Sandboxed Lua for `config.lua` and executable plugins.
//!
//! `config.lua` sits next to `config.json`. When the confirm card has
//! approved the file's SHA-256, it runs once and its table is merged over
//! the JSON. A syntax or runtime error is logged, shown in the window, and
//! the JSON config is what runs. The process does not panic.
//!
//! Executable plugins are `<name>.lua` files in the directory
//! `lua_plugins_dir` names. They are off until that directory is set, the
//! stem is listed in `lua_plugins`, and the same hash approval exists.
//! `bayan.spawn` exists only when that entry's `spawn` flag is true, and it
//! is never installed in the `config.lua` state.
//!
//! # Plugin API
//!
//! ```lua
//! bayan.bind("ctrl+alt+g", function()
//!   bayan.send("git status\r")
//! end)
//!
//! bayan.on("tab-opened", function(ev)
//!   -- ev.event, ev.index
//! end)
//! ```
//!
//! `bayan.bind(chord, fn)` registers a keybinding. The chord is the same
//! form as config.json (`ctrl+alt+g`, `f12`). It must carry a modifier or
//! be an F-key, so a plugin cannot swallow ordinary typing. A built-in
//! chord still wins.
//!
//! `bayan.on(event, fn)` subscribes to `tab-opened`, `title-changed`,
//! `bell`, or `osc7-cwd`. The argument is a table with `event` and any
//! extra string fields. Bayan dispatches `tab-opened` when the user opens
//! a tab. It does not dispatch the others from terminal output: bytes from
//! the pane are never passed to Lua.
//!
//! `bayan.send(text)` queues text for the active pane. The text waits on
//! the same confirm card as a JSON plugin action; it is not written to the
//! shell immediately. CR and tab are allowed. Other control, bidi, and
//! zero-width characters are rejected, and the text is capped at the same
//! length as a JSON action.
//!
//! `bayan.spawn(program, args)` starts `program` with `args` and no shell,
//! then returns `""`. Stdout is discarded. The process is killed after two
//! seconds, so this is not a way to launch an editor. At most four such
//! processes run at once. On Windows the process is created suspended, put
//! in a job, and only then resumed, so it cannot outlive the job handle.
//! It is absent unless this plugin's allowlist entry sets `"spawn": true`.
//!
//! Each call into a plugin — loading it, a key binding, one event dispatch —
//! gets its own instruction budget and its own deadline. Handlers of one
//! dispatch share that deadline, and one event holds at most 32 of them.
//! A loop is cut off. The next call starts clean. `__gc` and `__close` are
//! rejected. A metatable installed by `setmetatable` is a private copy, so
//! a later write of `__close` cannot reach it, including the fourth value
//! of a generic `for`. `<close>` (`OP_TBC`) is also refused in every loaded
//! chunk, including one hidden inside `load`.
//!
//! Strings crossing the boundary are UTF-8. Arabic round-trips unchanged.
//!
//! An elevated process token — `--elevated` after UAC, or Explorer's Run as
//! administrator — loads none of this. [`is_elevated`] reads the token.
//! [`load_with`] takes that answer so tests can force either side. The JSON
//! config is kept and a short Arabic notice is returned; no script is read.

use std::cell::RefCell;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::chunk::ChunkMode;
use mlua::{HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Value, VmState};
use sha2::{Digest, Sha256};

use crate::config::{LuaPluginAllow, UserConfig};

/// Keys `config.lua` is not allowed to change. The allowlist, the spawn
/// flags, the plugins directory, and the approval hashes stay the values
/// from config.json.
const SECURITY_KEYS: [&str; 3] = ["lua_approvals", "lua_plugins", "lua_plugins_dir"];

/// A Lua file larger than this is refused before the VM sees it.
const MAX_SOURCE: u64 = 256 * 1024;

/// 8 MiB for the whole state, including the libraries.
const MEMORY_LIMIT: usize = 8 * 1024 * 1024;
/// Hook cadence. A tight loop trips the budget well under the deadline.
const INSTRUCTION_STRIDE: u32 = 1_000;
const INSTRUCTION_BUDGET: u64 = 1_000_000;
const DEADLINE: Duration = Duration::from_millis(100);

const EVENTS: [&str; 4] = ["tab-opened", "title-changed", "bell", "osc7-cwd"];
/// Handlers past this for one event are refused. Each one used to get a
/// full deadline, so a few thousand spinning handlers froze a tab open.
const MAX_HANDLERS_PER_EVENT: usize = 32;
/// Processes `bayan.spawn` may have started and not yet reaped.
const MAX_SPAWNS: usize = 4;
/// `string.find` and its siblings match in one C call, which the hook cannot
/// interrupt. A call whose worst case (`pattern_cost`) is over this many
/// matcher steps is refused before it starts.
const PATTERN_BUDGET: f64 = 1e7;
/// `string.rep` and `table.move` loop in C too. `string.rep("", n)` copies
/// zero bytes n times, so the memory limit never stops it.
const MAX_REP: u128 = 1 << 20;
const MAX_MOVE: i128 = 1 << 20;
/// config.lua's tables become JSON by Rust recursion, outside the hook.
/// A cycle or deep nesting would overflow the stack on every launch, and
/// shared subtables would be copied once per path.
const MAX_CONFIG_DEPTH: usize = 32;
const MAX_CONFIG_VALUES: usize = 100_000;

struct Limits {
    memory: usize,
    stride: u32,
    instructions: u64,
    deadline: Duration,
}

impl Limits {
    fn production() -> Self {
        Self {
            memory: MEMORY_LIMIT,
            stride: INSTRUCTION_STRIDE,
            instructions: INSTRUCTION_BUDGET,
            deadline: DEADLINE,
        }
    }
}

struct Fuel {
    /// Reset before every entry from the host. The hook reads it.
    start: Mutex<Instant>,
    steps: AtomicU64,
    stride: u64,
    instructions: u64,
    deadline: Duration,
    exhausted: AtomicBool,
}

impl Fuel {
    fn new(limits: &Limits) -> Arc<Self> {
        Arc::new(Self {
            start: Mutex::new(Instant::now()),
            steps: AtomicU64::new(0),
            stride: u64::from(limits.stride),
            instructions: limits.instructions,
            deadline: limits.deadline,
            exhausted: AtomicBool::new(false),
        })
    }

    /// A fresh deadline and a fresh instruction budget for this call only.
    fn reset(&self) {
        let mut start = self.start.lock().unwrap_or_else(|e| e.into_inner());
        *start = Instant::now();
        self.steps.store(0, Ordering::Relaxed);
        self.exhausted.store(false, Ordering::Release);
    }

    fn elapsed(&self) -> Duration {
        self.start
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }

    fn over(&self) -> bool {
        if self.exhausted.load(Ordering::Acquire) {
            return true;
        }
        let steps = self.steps.load(Ordering::Relaxed);
        steps.saturating_mul(self.stride) > self.instructions || self.elapsed() > self.deadline
    }

    fn mark_if_over(&self, steps: u64) -> bool {
        let over = self.exhausted.load(Ordering::Acquire)
            || steps.saturating_mul(self.stride) > self.instructions
            || self.elapsed() > self.deadline;
        if over {
            self.exhausted.store(true, Ordering::Release);
        }
        over
    }
}

/// What a file is, for the confirm card and for the grant that follows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptRole {
    Config,
    Plugin { name: String, spawn: bool },
}

/// A Lua file the confirm card has to approve before it may run.
#[derive(Clone, Debug)]
pub struct PendingScript {
    pub role: ScriptRole,
    pub path: PathBuf,
    pub hash: String,
    /// What the card is showing. Short files are the whole source. A longer
    /// file starts as a head plus the hash and size; [`Self::reveal`] replaces
    /// it with the whole source before Enter may approve.
    pub preview: String,
    full: String,
    bytes: usize,
    line_count: usize,
    /// True only when `preview` contains the entire source.
    complete: bool,
}

impl PendingScript {
    pub fn reason(&self) -> String {
        let which = match &self.role {
            ScriptRole::Config => crate::lang::tr("config.lua", "config.lua"),
            ScriptRole::Plugin { name, .. } => name.as_str(),
        };
        format!(
            "{} — {which} — {} {}",
            crate::lang::tr("تشغيل سكربت لوا", "Run a Lua script"),
            self.bytes,
            crate::lang::tr("بايت", "bytes"),
        )
    }

    /// Enter may approve only a card that already shows the whole file.
    pub fn shows_everything(&self) -> bool {
        self.complete
    }

    /// Replace the truncated card with the full source, the hash, and the size.
    pub fn reveal(mut self) -> Self {
        self.preview = format!(
            "sha256:{}\n{} bytes · {} lines\n{}",
            self.hash, self.bytes, self.line_count, self.full
        );
        self.complete = true;
        self
    }
}

/// Text a plugin callback queued for the confirm card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginSend {
    pub plugin: String,
    pub text: String,
}

struct PluginRegs {
    bindings: Rc<RefCell<Vec<(String, mlua::RegistryKey)>>>,
    handlers: Rc<RefCell<Vec<(String, mlua::RegistryKey)>>>,
    sends: Arc<Mutex<Vec<String>>>,
    runs: Arc<AtomicU64>,
}

struct LoadedPlugin {
    name: String,
    bindings: Rc<RefCell<Vec<(String, mlua::RegistryKey)>>>,
    handlers: Rc<RefCell<Vec<(String, mlua::RegistryKey)>>>,
    sends: Arc<Mutex<Vec<String>>>,
    runs: Arc<AtomicU64>,
    fuel: Arc<Fuel>,
    lua: Lua,
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        // Registry keys unref through the Lua state. Drop them while it
        // is still alive; the closures only hold the Rc, not the keys.
        self.bindings.borrow_mut().clear();
        self.handlers.borrow_mut().clear();
    }
}

/// The executable plugins that were approved and ran.
#[derive(Default)]
pub struct PluginSet {
    plugins: Vec<LoadedPlugin>,
}

impl PluginSet {
    pub fn has_binding(&self, chord: &str) -> bool {
        self.plugins
            .iter()
            .any(|p| p.bindings.borrow().iter().any(|(c, _)| c == chord))
    }

    /// Run every callback bound to `chord`. Terminal bytes never arrive here.
    pub fn fire(&mut self, chord: &str) -> Vec<PluginSend> {
        let mut out = Vec::new();
        let started = Instant::now();
        for plugin in &self.plugins {
            if started.elapsed() > DEADLINE {
                break;
            }
            let hits: Vec<usize> = plugin
                .bindings
                .borrow()
                .iter()
                .enumerate()
                .filter(|(_, (c, _))| c == chord)
                .map(|(i, _)| i)
                .collect();
            if hits.is_empty() {
                continue;
            }
            // One budget for every binding this chord runs on this plugin.
            plugin.fuel.reset();
            for index in hits {
                if started.elapsed() > DEADLINE || plugin.fuel.over() {
                    break;
                }
                call_binding(plugin, index);
                out.extend(drain_sends(plugin));
            }
        }
        out
    }

    /// Dispatch a host event. Callers must not use this for PTY bytes.
    /// Every handler of this call shares one deadline.
    pub fn emit(&mut self, event: &str, fields: &[(&str, &str)]) -> Vec<PluginSend> {
        let mut out = Vec::new();
        let started = Instant::now();
        for plugin in &self.plugins {
            if started.elapsed() > DEADLINE {
                break;
            }
            let hits: Vec<usize> = plugin
                .handlers
                .borrow()
                .iter()
                .enumerate()
                .filter(|(_, (name, _))| name == event)
                .map(|(i, _)| i)
                .collect();
            if hits.is_empty() {
                continue;
            }
            plugin.fuel.reset();
            for index in hits {
                if started.elapsed() > DEADLINE || plugin.fuel.over() {
                    break;
                }
                call_handler(plugin, index, event, fields);
                out.extend(drain_sends(plugin));
            }
        }
        out
    }

    /// How many plugin callbacks have run. Terminal intake does not move it.
    #[cfg(test)]
    pub fn callback_runs(&self) -> u64 {
        self.plugins
            .iter()
            .map(|p| p.runs.load(Ordering::Relaxed))
            .sum()
    }
}

/// Facts parsed out of terminal output. They are not plugin events.
#[derive(Clone, Debug, Default)]
pub struct PtyFacts {
    pub bell: bool,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub hyperlink: bool,
    pub output: bool,
}

/// What a PTY fact is allowed to do to plugins: nothing.
///
/// The bell, title, cwd, and notify handlers call this and ignore the
/// result. Returning a send here would mean terminal output ran Lua, and
/// `terminal_output_never_runs_lua` fails.
pub fn plugin_sends_from_pty(plugins: &PluginSet, facts: &PtyFacts) -> Vec<PluginSend> {
    // Every fact is looked at and then dropped. Forwarding any of them into
    // `plugins.emit` is what `terminal_output_never_runs_lua` forbids.
    let _ = (
        plugins,
        facts.bell,
        facts.title.as_deref(),
        facts.cwd.as_deref(),
        facts.hyperlink,
        facts.output,
    );
    Vec::new()
}

fn call_binding(plugin: &LoadedPlugin, index: usize) {
    let func = {
        let bindings = plugin.bindings.borrow();
        let Some(key) = bindings.get(index).map(|(_, key)| key) else {
            return;
        };
        match plugin.lua.registry_value::<mlua::Function>(key) {
            Ok(func) => func,
            Err(err) => {
                log_line(&format!("{}: binding: {err}", plugin.name));
                return;
            }
        }
    };
    if plugin.fuel.over() {
        return;
    }
    plugin.runs.fetch_add(1, Ordering::Relaxed);
    if let Err(err) = func.call::<()>(()) {
        log_line(&format!("{}: binding: {err}", plugin.name));
    }
}

fn call_handler(plugin: &LoadedPlugin, index: usize, event: &str, fields: &[(&str, &str)]) {
    let func = {
        let handlers = plugin.handlers.borrow();
        let Some(key) = handlers.get(index).map(|(_, k)| k) else {
            return;
        };
        match plugin.lua.registry_value::<mlua::Function>(key) {
            Ok(func) => func,
            Err(err) => {
                log_line(&format!("{}: handler: {err}", plugin.name));
                return;
            }
        }
    };
    if plugin.fuel.over() {
        return;
    }
    plugin.runs.fetch_add(1, Ordering::Relaxed);
    let info = match plugin.lua.create_table() {
        Ok(t) => t,
        Err(err) => {
            log_line(&format!("{}: handler: {err}", plugin.name));
            return;
        }
    };
    if let Err(err) = (|| {
        info.set("event", event)?;
        for (k, v) in fields {
            info.set(*k, *v)?;
        }
        func.call::<()>(info)
    })() {
        log_line(&format!("{}: {event}: {err}", plugin.name));
    }
}

fn drain_sends(plugin: &LoadedPlugin) -> Vec<PluginSend> {
    let queued = match plugin.sends.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(poison) => std::mem::take(&mut *poison.into_inner()),
    };
    queued
        .into_iter()
        .map(|text| PluginSend {
            plugin: plugin.name.clone(),
            text,
        })
        .collect()
}

/// Shown in an elevated window. Scripts and plugins stay off; the JSON
/// config is what runs. Arabic on purpose: the window's own notice.
pub const ELEVATED_NOTICE: &str = "سكربتات لوا والإضافات متوقفة لأن النافذة تعمل كمسؤول.";

/// Result of loading `config.lua` and the executable plugins.
pub struct Outcome {
    /// Runtime settings: JSON plus an approved `config.lua`.
    pub config: UserConfig,
    /// The document to write back. Lua overrides are not in it, so deleting
    /// `config.lua` drops them on the next load.
    pub persisted: UserConfig,
    /// Technical failures. Logged, not shown.
    pub errors: Vec<String>,
    /// Arabic sentences for the window. The technical text stays in `errors`.
    pub shown: Vec<String>,
    /// A policy notice, not a script failure. Set when the token is elevated
    /// and the user actually has a script or a plugin.
    pub notice: Option<String>,
    pub pending: Vec<PendingScript>,
    pub plugins: PluginSet,
}

/// User Lua and every plugin (executable or declarative) load only when
/// `elevated` is false. Tests pass the flag; the process asks [`is_elevated`].
pub fn scripts_enabled(elevated: bool) -> bool {
    !elevated
}

/// Whether this process token is elevated. The query lives in [`crate::elevate`]
/// so a later merge with the `--elevated` relaunch uses one check. A failed
/// query is elevated (scripts stay off). Tests pass the boolean into
/// [`load_with`] instead of calling this.
pub fn is_elevated() -> bool {
    // `process_is_elevated` is the name #56 uses. Same function.
    crate::elevate::process_is_elevated()
}

/// Load Lua beside `dir`'s config.json. Unapproved and changed files are
/// returned in `pending` and are not executed. This asks [`is_elevated`]
/// itself, so a config watcher has no other entry and cannot run a file
/// while the token is elevated.
pub fn load_at(cfg: &UserConfig, dir: &Path) -> Outcome {
    load_with(cfg, dir, is_elevated())
}

/// [`load_at`] with the elevation answer supplied by the caller. `true`
/// keeps the JSON config and runs no Lua and no plugin, whatever files
/// exist under `dir` or `lua_plugins_dir`.
pub fn load_with(cfg: &UserConfig, dir: &Path, elevated: bool) -> Outcome {
    if !scripts_enabled(elevated) {
        let notice = user_has_scripts(cfg, dir).then(|| {
            log_line(ELEVATED_NOTICE);
            ELEVATED_NOTICE.to_string()
        });
        return Outcome {
            config: cfg.clone(),
            persisted: cfg.clone(),
            errors: Vec::new(),
            shown: Vec::new(),
            notice,
            pending: Vec::new(),
            plugins: PluginSet::default(),
        };
    }
    let persisted = cfg.clone();
    let mut errors = Vec::new();
    let mut shown = Vec::new();
    let mut pending = Vec::new();
    let mut config = cfg.clone();

    let lua_path = dir.join("config.lua");
    match consider_config(cfg, &lua_path) {
        ConfigLua::Absent => {}
        ConfigLua::Pending(script) => {
            log_line(&format!(
                "config.lua is waiting for approval ({})",
                script.hash
            ));
            pending.push(script);
        }
        ConfigLua::Failed(err) => {
            let err = format!("config.lua: {err}");
            log_line(&err);
            shown.push(arabic_notice(&err));
            errors.push(err);
        }
        ConfigLua::Applied(merged) => config = *merged,
    }

    // Allowlist, directory, and approvals come from the JSON config, not
    // from whatever config.lua just merged.
    let plugins = load_plugins(cfg, dir, &mut pending, &mut errors, &mut shown);
    Outcome {
        config,
        persisted,
        errors,
        shown,
        notice: None,
        pending,
        plugins,
    }
}

/// What the config watcher (PR #52) must call when that lands. It must not
/// run Lua itself. A new or changed file comes back in `pending`, and an
/// elevated token runs nothing.
pub fn reload(cfg: &UserConfig, dir: &Path) -> Outcome {
    load_at(cfg, dir)
}

/// Record an approval. The caller saves the config; a later [`load_at`]
/// then runs the file.
pub fn grant(cfg: &mut UserConfig, hash: &str) {
    if !cfg.lua_approvals.iter().any(|h| h == hash) {
        cfg.lua_approvals.push(hash.to_string());
    }
}

/// True when `path` still has the bytes the card showed.
pub fn unchanged(script: &PendingScript) -> Result<(), String> {
    let meta = fs::symlink_metadata(&script.path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("refusing a symlink".into());
    }
    let bytes = fs::read(&script.path).map_err(|e| e.to_string())?;
    if content_hash(&bytes) != script.hash {
        return Err("the file changed and needs a new approval".into());
    }
    Ok(())
}

/// SHA-256 of the file bytes, lowercase hex.
pub fn content_hash(bytes: &[u8]) -> String {
    let dig = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for byte in dig {
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0xf) as usize] as char);
    }
    s
}

const HEX: &[u8; 16] = b"0123456789abcdef";

enum ConfigLua {
    Absent,
    Pending(PendingScript),
    Applied(Box<UserConfig>),
    Failed(String),
}

fn consider_config(cfg: &UserConfig, path: &Path) -> ConfigLua {
    if !path.exists() {
        return ConfigLua::Absent;
    }
    let bytes = match read_source(path) {
        Ok(b) => b,
        Err(err) => return ConfigLua::Failed(err),
    };
    let text = match source_text(&bytes) {
        Ok(t) => t.to_string(),
        Err(err) => return ConfigLua::Failed(err),
    };
    let hash = content_hash(&bytes);
    if !approved(cfg, &hash) {
        return ConfigLua::Pending(pending_script(ScriptRole::Config, path, &text, hash));
    }
    match apply_config_lua(cfg, &bytes) {
        Ok(merged) => ConfigLua::Applied(Box::new(merged)),
        Err(err) => ConfigLua::Failed(err),
    }
}

fn approved(cfg: &UserConfig, hash: &str) -> bool {
    cfg.lua_approvals.iter().any(|h| h == hash)
}

fn pending_script(role: ScriptRole, path: &Path, text: &str, hash: String) -> PendingScript {
    let bytes = text.len();
    let line_count = text.lines().count().max(1);
    let chars = text.chars().count();
    let header = format!("sha256:{hash}\n{bytes} bytes · {line_count} lines\n");
    // 12 card rows. A longer file is not approvable until reveal() shows it all.
    let complete = chars <= 12 * 56;
    let preview = if complete {
        format!("{header}{text}")
    } else {
        let head: String = text.chars().take(12 * 56).collect();
        format!("{header}{head}\n…")
    };
    PendingScript {
        role,
        path: path.to_path_buf(),
        hash,
        preview,
        full: text.to_string(),
        bytes,
        line_count,
        complete,
    }
}

fn arabic_notice(technical: &str) -> String {
    let t = technical.to_ascii_lowercase();
    if t.contains("limit") {
        "تجاوز السكربت حد الوقت أو التعليمات.".into()
    } else if t.contains("memory") || t.contains("not enough memory") {
        "تجاوز السكربت حد الذاكرة.".into()
    } else if t.contains("syntax") || t.contains("unexpected") || t.contains("near") {
        "في السكربت خطأ صياغة.".into()
    } else if t.contains("symlink") {
        "لن يُشغَّل اختصار رمزي.".into()
    } else {
        "تعذر تشغيل السكربت. التفاصيل في سجل lua.log.".into()
    }
}

/// True when an elevated window has something to refuse. An empty home
/// does not get the notice.
fn user_has_scripts(cfg: &UserConfig, dir: &Path) -> bool {
    if dir.join("config.lua").is_file() || !cfg.lua_plugins.is_empty() {
        return true;
    }
    if dir_has_plugins(&dir.join("plugins")) {
        return true;
    }
    plugins_root(cfg, dir).is_some_and(|root| dir_has_plugins(&root))
}

fn dir_has_plugins(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        e.path()
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("lua") || x.eq_ignore_ascii_case("json"))
    })
}

fn apply_config_lua(cfg: &UserConfig, bytes: &[u8]) -> Result<UserConfig, String> {
    let source = source_text(bytes)?;
    let original = serde_json::to_value(cfg).map_err(|e| e.to_string())?;
    let (lua, fuel) = open_sandbox(Limits::production())?;
    let table = json_to_lua(&lua, &original)?;
    let Value::Table(config_table) = table else {
        return Err("config did not become a table".into());
    };
    lua.globals()
        .set("config", config_table)
        .map_err(|e| e.to_string())?;
    // No bayan table. spawn, bind, on, and send are not installed.
    fuel.reset();
    let returned: Value = compile_text(&lua, source, "config.lua")?
        .call(())
        .map_err(|e| e.to_string())?;
    let mut merged = {
        let current: Value = lua.globals().get("config").map_err(|e| e.to_string())?;
        lua_to_json(current)?
    };
    match returned {
        Value::Nil => {}
        Value::Table(_) => {
            let overlay = lua_to_json(returned)?;
            deep_merge(&mut merged, overlay);
        }
        _ => return Err("config.lua must return a table or nothing".into()),
    }
    restore_security(&mut merged, &original);
    serde_json::from_value(merged).map_err(|e| format!("merged config is not valid: {e}"))
}

fn load_plugins(
    cfg: &UserConfig,
    bayan_dir: &Path,
    pending: &mut Vec<PendingScript>,
    errors: &mut Vec<String>,
    shown: &mut Vec<String>,
) -> PluginSet {
    if cfg.lua_plugins.is_empty() {
        return PluginSet::default();
    }
    let Some(root) = plugins_root(cfg, bayan_dir) else {
        let msg = "lua_plugins is set but lua_plugins_dir is not; no executable plugin ran";
        log_line(msg);
        shown.push(arabic_notice(msg));
        errors.push(msg.into());
        return PluginSet::default();
    };
    let mut set = PluginSet::default();
    for allow in &cfg.lua_plugins {
        match load_one(cfg, &root, allow) {
            Ok(OnePlugin::Pending(script)) => pending.push(script),
            Ok(OnePlugin::Loaded(plugin)) => set.plugins.push(plugin),
            Err(err) => {
                let err = format!("{}: {err}", allow.name);
                log_line(&err);
                shown.push(arabic_notice(&err));
                errors.push(err);
            }
        }
    }
    set
}

enum OnePlugin {
    Pending(PendingScript),
    Loaded(LoadedPlugin),
}

fn load_one(cfg: &UserConfig, root: &Path, allow: &LuaPluginAllow) -> Result<OnePlugin, String> {
    let path = plugin_path(root, &allow.name)?;
    let bytes = read_source(&path)?;
    let text = source_text(&bytes)?.to_string();
    let hash = content_hash(&bytes);
    if !approved(cfg, &hash) {
        log_line(&format!("{}: waiting for approval ({hash})", allow.name));
        return Ok(OnePlugin::Pending(pending_script(
            ScriptRole::Plugin {
                name: allow.name.clone(),
                spawn: allow.spawn,
            },
            &path,
            &text,
            hash,
        )));
    }
    let plugin = exec_plugin(allow, &text)?;
    Ok(OnePlugin::Loaded(plugin))
}

fn plugins_root(cfg: &UserConfig, bayan_dir: &Path) -> Option<PathBuf> {
    let raw = cfg.lua_plugins_dir.as_ref()?;
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        bayan_dir.join(path)
    };
    Some(path)
}

/// The file must be `<root>/<name>.lua`, not a symlink, and not a path that
/// leaves `root` after the directory is canonicalized.
fn plugin_path(root: &Path, name: &str) -> Result<PathBuf, String> {
    if !stem_ok(name) {
        return Err(format!("plugin name {name:?} is not a single file stem"));
    }
    let root = fs::canonicalize(root).map_err(|e| format!("plugins dir: {e}"))?;
    if !root.is_dir() {
        return Err("plugins path is not a directory".into());
    }
    let path = root.join(format!("{name}.lua"));
    let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("refusing a symlink".into());
    }
    if !meta.is_file() {
        return Err("not a file".into());
    }
    let canon = fs::canonicalize(&path).map_err(|e| e.to_string())?;
    if canon.parent() != Some(root.as_path()) {
        return Err("plugin is outside the configured directory".into());
    }
    Ok(canon)
}

fn stem_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':', '\0'])
        && name != "."
        && name != ".."
        && !name.starts_with('.')
}

fn exec_plugin(allow: &LuaPluginAllow, source: &str) -> Result<LoadedPlugin, String> {
    let (lua, fuel) = open_sandbox(Limits::production())?;
    let regs = PluginRegs {
        bindings: Rc::new(RefCell::new(Vec::new())),
        handlers: Rc::new(RefCell::new(Vec::new())),
        sends: Arc::new(Mutex::new(Vec::new())),
        runs: Arc::new(AtomicU64::new(0)),
    };
    install_plugin_api(&lua, allow.spawn, &regs)?;
    fuel.reset();
    compile_text(&lua, source, &allow.name)?
        .call::<()>(())
        .map_err(|e| e.to_string())?;
    Ok(LoadedPlugin {
        name: allow.name.clone(),
        bindings: regs.bindings,
        handlers: regs.handlers,
        sends: regs.sends,
        runs: regs.runs,
        fuel,
        lua,
    })
}

fn install_plugin_api(lua: &Lua, spawn: bool, regs: &PluginRegs) -> Result<(), String> {
    let bayan = lua.create_table().map_err(|e| e.to_string())?;
    let bindings = Rc::clone(&regs.bindings);
    bayan
        .set(
            "bind",
            lua.create_function(move |lua, (spec, func): (String, mlua::Function)| {
                let chord = crate::keybinds::Chord::parse(&spec)
                    .filter(|c| c.is_bindable())
                    .ok_or_else(|| {
                        mlua::Error::runtime(format!("chord {spec:?} is not bindable"))
                    })?;
                let canon = chord.to_config();
                let key = lua.create_registry_value(func)?;
                let mut list = bindings.borrow_mut();
                if list.iter().any(|(c, _)| c == &canon) {
                    return Err(mlua::Error::runtime(format!(
                        "chord {canon} is already bound"
                    )));
                }
                list.push((canon, key));
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let handlers = Rc::clone(&regs.handlers);
    bayan
        .set(
            "on",
            lua.create_function(move |lua, (event, func): (String, mlua::Function)| {
                if !EVENTS.contains(&event.as_str()) {
                    return Err(mlua::Error::runtime(format!("unknown event {event:?}")));
                }
                let mut list = handlers.borrow_mut();
                let count = list.iter().filter(|(name, _)| name == &event).count();
                if count >= MAX_HANDLERS_PER_EVENT {
                    return Err(mlua::Error::runtime(format!(
                        "too many handlers for {event}"
                    )));
                }
                let key = lua.create_registry_value(func)?;
                list.push((event, key));
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    let sends = Arc::clone(&regs.sends);
    bayan
        .set(
            "send",
            lua.create_function(move |_, text: String| {
                check_send(&text).map_err(mlua::Error::runtime)?;
                match sends.lock() {
                    Ok(mut g) => g.push(text),
                    Err(poison) => poison.into_inner().push(text),
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

    if spawn {
        bayan
            .set(
                "spawn",
                lua.create_function(|_, (program, args): (String, mlua::Table)| {
                    if program.is_empty() || program.contains('\0') {
                        return Err(mlua::Error::runtime(
                            "spawn program is empty or contains NUL",
                        ));
                    }
                    let args = strings_from_table(args)?;
                    spawn_process(&program, &args).map_err(mlua::Error::runtime)
                })
                .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
    }

    lua.globals().set("bayan", bayan).map_err(|e| e.to_string())
}

fn strings_from_table(t: mlua::Table) -> mlua::Result<Vec<String>> {
    let n = t.raw_len();
    if n > 64 {
        return Err(mlua::Error::runtime("too many spawn arguments"));
    }
    let mut out = Vec::with_capacity(n);
    for i in 1..=n {
        let s: String = t.get(i)?;
        if s.contains('\0') || s.chars().count() > 4096 {
            return Err(mlua::Error::runtime(
                "spawn argument is empty, huge, or contains NUL",
            ));
        }
        out.push(s);
    }
    Ok(out)
}

static SPAWNS: AtomicUsize = AtomicUsize::new(0);

/// True when this call reserved one of [`MAX_SPAWNS`] slots.
fn try_begin_spawn(slot: &AtomicUsize) -> bool {
    let mut cur = slot.load(Ordering::Relaxed);
    loop {
        if cur >= MAX_SPAWNS {
            return false;
        }
        match slot.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(seen) => cur = seen,
        }
    }
}

fn end_spawn(slot: &AtomicUsize) {
    slot.fetch_sub(1, Ordering::AcqRel);
}

fn spawn_process(program: &str, args: &[String]) -> Result<String, String> {
    if !try_begin_spawn(&SPAWNS) {
        return Err("too many spawns".into());
    }
    match spawn_started(program, args) {
        Ok(child) => {
            std::thread::spawn(move || {
                supervise_child(child.child, child.job);
                end_spawn(&SPAWNS);
            });
            Ok(String::new())
        }
        Err(err) => {
            end_spawn(&SPAWNS);
            Err(err)
        }
    }
}

struct Started {
    child: std::process::Child,
    job: Option<JobGuard>,
}

fn spawn_started(program: &str, args: &[String]) -> Result<Started, String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_SUSPENDED | CREATE_NO_WINDOW. The process stays frozen
        // until it is inside the job, and no console window flashes.
        cmd.creation_flags(0x0000_0004 | 0x0800_0000);
    }
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    let job = {
        let job = tie_to_job(&child);
        resume_process(&child);
        job
    };
    #[cfg(not(windows))]
    let job = None;
    Ok(Started { child, job })
}

fn supervise_child(mut child: std::process::Child, job: Option<JobGuard>) {
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout {
            let mut tmp = [0u8; 4096];
            while buf.len() < 64 * 1024 {
                match out.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = 64 * 1024 - buf.len();
                        buf.extend_from_slice(&tmp[..n.min(room)]);
                    }
                }
            }
        }
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(Duration::from_secs(2));
    let _ = child.kill();
    let _ = child.wait();
    drop(job);
}

/// A job that kills the process and anything it spawned when the handle
/// closes. Absent off Windows, and best-effort if the job cannot be made.
/// Called while the process is still suspended, so the job owns it before
/// the first instruction runs.
fn tie_to_job(_child: &std::process::Child) -> Option<JobGuard> {
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // SAFETY: CreateJobObjectW returns a handle this function either
        // closes or wraps in OwnedHandle. AssignProcessToJobObject borrows
        // the process handle owned by `Child`. The information pointer is a
        // local struct that outlives SetInformationJobObject.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
            {
                let _ = CloseHandle(job);
                return None;
            }
            if AssignProcessToJobObject(job, _child.as_raw_handle()) == 0 {
                let _ = CloseHandle(job);
                return None;
            }
            Some(JobGuard(OwnedHandle::from_raw_handle(job)))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = _child;
        None
    }
}

/// Std closes the primary thread when CREATE_SUSPENDED is set and does not
/// resume it. Resume the whole process after the job assignment.
#[cfg(windows)]
fn resume_process(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: windows_sys::Win32::Foundation::HANDLE) -> i32;
    }
    // SAFETY: the handle is the live process from Command::spawn.
    unsafe {
        let _ = NtResumeProcess(child.as_raw_handle());
    }
}

#[cfg(windows)]
struct JobGuard(std::os::windows::io::OwnedHandle);
#[cfg(not(windows))]
struct JobGuard;

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        // The field is the owned job handle. Dropping it after this function
        // closes the handle, and that close kills the process tree.
        let _ = self.0.as_raw_handle();
    }
}

fn check_send(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("send is empty".into());
    }
    if text.chars().any(send_char_forbidden) {
        return Err("send carries control or hidden characters".into());
    }
    if text.chars().count() > crate::plugins::ACTION_SEND_MAX {
        return Err("send is too long".into());
    }
    Ok(())
}

fn send_char_forbidden(c: char) -> bool {
    !matches!(c, '\r' | '\t') && crate::link::is_forbidden(c)
}

/// Build the sandbox. `Lua::new()` is not used: it loads `StdLib::ALL_SAFE`,
/// which includes `io`, `os`, and `package`. `new_with` still opens the base
/// library (`luaopen_base` in mlua 0.12.2), so `load`, `loadfile`, `dofile`,
/// and `require` are removed here, and `load` is replaced with a function
/// that forces text mode.
fn open_sandbox(limits: Limits) -> Result<(Lua, Arc<Fuel>), String> {
    let libs = StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8;
    // LuaOptions::default() in 0.12.2 (async feature off) is
    // catch_rust_panics: true. Panics in a callback become Lua errors
    // instead of unwinding through the C VM.
    let lua = Lua::new_with(libs, LuaOptions::default()).map_err(|e| e.to_string())?;
    lua.set_memory_limit(limits.memory)
        .map_err(|e| e.to_string())?;

    let fuel = Fuel::new(&limits);
    let hook_fuel = Arc::clone(&fuel);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(limits.stride),
        move |_lua, _dbg| {
            let steps = hook_fuel.steps.fetch_add(1, Ordering::Relaxed) + 1;
            if hook_fuel.mark_if_over(steps) {
                Err(mlua::Error::runtime("lua execution limit exceeded"))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
    .map_err(|e| e.to_string())?;

    harden(&lua, &fuel)?;
    Ok((lua, fuel))
}

fn harden(lua: &Lua, fuel: &Arc<Fuel>) -> Result<(), String> {
    let g = lua.globals();
    for name in [
        "io", "os", "debug", "package", "require", "loadfile", "dofile", "xpcall",
    ] {
        g.set(name, Value::Nil).map_err(|e| e.to_string())?;
    }
    let pcall_fuel = Arc::clone(fuel);
    g.set(
        "pcall",
        lua.create_function(move |lua, args: MultiValue| guarded_pcall(lua, &pcall_fuel, args))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    g.set(
        "load",
        lua.create_function(|lua, args: MultiValue| text_only_load(lua, args))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let string: mlua::Table = g.get("string").map_err(|e| e.to_string())?;
    string
        .set(
            "dump",
            lua.create_function(|_, _: MultiValue| -> mlua::Result<Value> {
                Err(mlua::Error::runtime("string.dump is disabled"))
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    // Lua 5.4 runs __gc and __close with hooks disabled. __gc is armed only
    // at setmetatable time. __close is not: a generic `for` marks its fourth
    // iterator value to-be-closed (OP_TFORPREP) and looks the metamethod up
    // then. The caller's table is therefore never the metatable that runs.
    let protected = lua.create_table().map_err(|e| e.to_string())?;
    // Weak keys. real, view, and guard are stored here, and a strong key
    // would keep every sealed metatable alive after its object is gone.
    let mode = lua.create_table().map_err(|e| e.to_string())?;
    mode.raw_set("__mode", "k").map_err(|e| e.to_string())?;
    protected
        .set_metatable(Some(mode))
        .map_err(|e| e.to_string())?;
    shield_shared_metatables(lua, &protected)?;
    let orig_setmt: mlua::Function = g.get("setmetatable").map_err(|e| e.to_string())?;
    let set_protected = protected.clone();
    g.set(
        "setmetatable",
        lua.create_function(move |lua, (obj, mt): (Value, Value)| match mt {
            Value::Table(src) => {
                let real = seal_user_metatable(lua, &set_protected, &src)?;
                orig_setmt.call::<Value>((obj, real))
            }
            other => orig_setmt.call::<Value>((obj, other)),
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let orig_rawset: mlua::Function = g.get("rawset").map_err(|e| e.to_string())?;
    let rawset_protected = protected.clone();
    g.set(
        "rawset",
        lua.create_function(move |_, (table, key, value): (Value, Value, Value)| {
            if meta_key(&key, "__gc") && !matches!(value, Value::Nil) {
                return Err(mlua::Error::runtime("__gc is disabled"));
            }
            if guarded(&rawset_protected, &table)
                && (meta_key(&key, "__close") || meta_key(&key, "__metatable"))
                && !matches!(value, Value::Nil)
            {
                return Err(mlua::Error::runtime("metatable is read-only"));
            }
            orig_rawset.call::<Value>((table, key, value))
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let orig_rawget: mlua::Function = g.get("rawget").map_err(|e| e.to_string())?;
    let rawget_protected = protected;
    g.set(
        "rawget",
        lua.create_function(move |_, (table, key): (Value, Value)| {
            // A protected metatable must not reveal a __close that the VM
            // would call, and rawget must not be a way to fetch one.
            if guarded(&rawget_protected, &table) && meta_key(&key, "__close") {
                return Ok(Value::Nil);
            }
            orig_rawget.call::<Value>((table, key))
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    // The string metatable's __index is this same table, so `s:find(...)`
    // reaches these wrappers too.
    cap_pattern_functions(lua, &string, fuel)?;
    cap_c_loops(lua, &string, &g, fuel)?;
    Ok(())
}

/// Tables whose metatable the sandbox owns: the private copy, the view
/// `getmetatable` returns, and the shared metatables Lua installs itself
/// (the string metatable in this build).
fn remember(protected: &mlua::Table, table: &mlua::Table) -> mlua::Result<()> {
    protected.raw_set(table.clone(), true)
}

fn guarded(protected: &mlua::Table, value: &Value) -> bool {
    let Value::Table(table) = value else {
        return false;
    };
    matches!(
        protected.raw_get::<Value>(table.clone()),
        Ok(Value::Boolean(true))
    )
}

fn meta_key(key: &Value, name: &str) -> bool {
    match key {
        Value::String(s) => s.to_str().ok().as_deref() == Some(name),
        _ => false,
    }
}

fn hidden_meta(key: &Value) -> bool {
    meta_key(key, "__close") || meta_key(key, "__gc") || meta_key(key, "__metatable")
}

fn copy_meta_fields(src: &mlua::Table, dst: &mlua::Table) -> mlua::Result<()> {
    for pair in src.pairs::<Value, Value>() {
        let (key, value) = pair?;
        if hidden_meta(&key) {
            continue;
        }
        dst.raw_set(key, value)?;
    }
    Ok(())
}

fn readonly_view(
    lua: &Lua,
    protected: &mlua::Table,
    src: &mlua::Table,
) -> mlua::Result<mlua::Table> {
    let view = lua.create_table()?;
    copy_meta_fields(src, &view)?;
    let guard = lua.create_table()?;
    guard.raw_set(
        "__newindex",
        lua.create_function(|_, _: (Value, Value, Value)| -> mlua::Result<()> {
            Err(mlua::Error::runtime("metatable is read-only"))
        })?,
    )?;
    guard.raw_set("__metatable", "protected")?;
    view.set_metatable(Some(guard.clone()))?;
    remember(protected, &view)?;
    remember(protected, &guard)?;
    Ok(view)
}

/// A private metatable with no `__close` or `__gc`. `getmetatable` returns
/// a read-only view, not this table, so a later write cannot arm a closer.
fn seal_user_metatable(
    lua: &Lua,
    protected: &mlua::Table,
    src: &mlua::Table,
) -> mlua::Result<mlua::Table> {
    if !matches!(src.raw_get::<Value>("__close")?, Value::Nil) {
        return Err(mlua::Error::runtime("__close is disabled"));
    }
    if !matches!(src.raw_get::<Value>("__gc")?, Value::Nil) {
        return Err(mlua::Error::runtime("__gc is disabled"));
    }
    let real = lua.create_table()?;
    copy_meta_fields(src, &real)?;
    let view = readonly_view(lua, protected, &real)?;
    real.raw_set("__metatable", view)?;
    remember(protected, &real)?;
    Ok(real)
}

fn shield_one(lua: &Lua, protected: &mlua::Table, real: &mlua::Table) -> mlua::Result<()> {
    real.raw_set("__close", Value::Nil)?;
    real.raw_set("__gc", Value::Nil)?;
    let view = readonly_view(lua, protected, real)?;
    real.raw_set("__metatable", view)?;
    remember(protected, real)?;
    Ok(())
}

/// String (and any other value Lua already gave a metatable) keep that
/// table, but it is no longer the one `getmetatable` hands back.
fn shield_shared_metatables(lua: &Lua, protected: &mlua::Table) -> Result<(), String> {
    let found: MultiValue = lua
        .load(
            "return getmetatable(''), getmetatable(0), getmetatable(true), getmetatable(function() end)",
        )
        .eval()
        .map_err(|e| e.to_string())?;
    for value in found {
        if let Value::Table(mt) = value {
            shield_one(lua, protected, &mt).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// The hook runs every `INSTRUCTION_STRIDE` instructions, and a loop body
/// calling a C function is a handful of them, so each capped call checks
/// the deadline itself. The overrun is then one call, not hundreds.
fn checkpoint(fuel: &Fuel) -> mlua::Result<()> {
    if fuel.mark_if_over(fuel.steps.load(Ordering::Relaxed)) {
        return Err(mlua::Error::runtime("lua execution limit exceeded"));
    }
    Ok(())
}

fn cap_pattern_functions(lua: &Lua, string: &mlua::Table, fuel: &Arc<Fuel>) -> Result<(), String> {
    for name in ["find", "match", "gmatch", "gsub"] {
        let orig: mlua::Function = string.get(name).map_err(|e| e.to_string())?;
        let fuel = Arc::clone(fuel);
        string
            .set(
                name,
                lua.create_function(move |lua, args: MultiValue| {
                    checkpoint(&fuel)?;
                    let args = args.into_vec();
                    let subject = arg_string(lua, &args, 0)?;
                    let pattern = arg_string(lua, &args, 1)?;
                    if let (Some(s), Some(p)) = (subject, pattern) {
                        let plain = name == "find"
                            && !matches!(
                                args.get(3),
                                None | Some(Value::Nil | Value::Boolean(false))
                            );
                        let cost = pattern_cost(
                            s.as_bytes().len(),
                            &p.as_bytes(),
                            plain,
                            name != "gmatch",
                        );
                        if cost > PATTERN_BUDGET {
                            return Err(mlua::Error::runtime(
                                "pattern is too costly for this string",
                            ));
                        }
                    }
                    orig.call::<MultiValue>(MultiValue::from_vec(args))
                })
                .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `string.rep` and `table.move`, bounded by how many times their C loop runs.
fn cap_c_loops(
    lua: &Lua,
    string: &mlua::Table,
    g: &mlua::Table,
    fuel: &Arc<Fuel>,
) -> Result<(), String> {
    let orig_rep: mlua::Function = string.get("rep").map_err(|e| e.to_string())?;
    let rep_fuel = Arc::clone(fuel);
    let rep = lua
        .create_function(move |lua, args: MultiValue| {
            checkpoint(&rep_fuel)?;
            let args = args.into_vec();
            let s = arg_string(lua, &args, 0)?.map(|s| s.as_bytes().len() as u128);
            let n = arg_integer(lua, &args, 1)?;
            let sep = arg_string(lua, &args, 2)?.map_or(0, |s| s.as_bytes().len() as u128);
            if let (Some(l), Some(n)) = (s, n) {
                if n > 0 {
                    let n = n as u128;
                    let total = n * l + (n - 1) * sep;
                    if total > MAX_REP {
                        return Err(mlua::Error::runtime("string.rep result is too long"));
                    }
                    if total == 0 {
                        return Ok(Value::String(lua.create_string("")?));
                    }
                }
            }
            orig_rep.call::<Value>(MultiValue::from_vec(args))
        })
        .map_err(|e| e.to_string())?;
    string.set("rep", rep).map_err(|e| e.to_string())?;

    let table: mlua::Table = g.get("table").map_err(|e| e.to_string())?;
    let orig_move: mlua::Function = table.get("move").map_err(|e| e.to_string())?;
    let move_fuel = Arc::clone(fuel);
    let mv = lua
        .create_function(move |lua, args: MultiValue| {
            checkpoint(&move_fuel)?;
            let args = args.into_vec();
            if let (Some(f), Some(e)) = (arg_integer(lua, &args, 1)?, arg_integer(lua, &args, 2)?) {
                if i128::from(e) - i128::from(f) >= MAX_MOVE {
                    return Err(mlua::Error::runtime("table.move: too many elements"));
                }
            }
            orig_move.call::<Value>(MultiValue::from_vec(args))
        })
        .map_err(|e| e.to_string())?;
    table.set("move", mv).map_err(|e| e.to_string())
}

/// Argument `i` as Lua's C side would read it; `None` lets the original
/// function raise its own type error.
fn arg_string(lua: &Lua, args: &[Value], i: usize) -> mlua::Result<Option<mlua::LuaString>> {
    match args.get(i) {
        Some(v @ (Value::String(_) | Value::Integer(_) | Value::Number(_))) => {
            lua.coerce_string(v.clone())
        }
        _ => Ok(None),
    }
}

fn arg_integer(lua: &Lua, args: &[Value], i: usize) -> mlua::Result<Option<i64>> {
    match args.get(i) {
        Some(v @ (Value::String(_) | Value::Integer(_) | Value::Number(_))) => {
            lua.coerce_integer(v.clone())
        }
        _ => Ok(None),
    }
}

/// An upper bound on the steps of Lua 5.4's backtracking matcher
/// (`lstrlib.c` `match`) over a subject of `n` bytes. Each start position
/// tries every split of the subject among the `q` quantified items
/// (`*` `+` `-` `?`). Their counts sum to at most `n`, so a start sees at
/// most C(n+q, q) splits, each walking at most `m` pattern bytes over q+1
/// nodes. `%b` and back-references rescan up to `n` bytes each. Plain
/// `find` is a memory search, `n * m`.
fn pattern_cost(n: usize, pat: &[u8], plain: bool, can_anchor: bool) -> f64 {
    let n1 = n as f64 + 1.0;
    let m = pat.len().max(1) as f64;
    if plain {
        return n1 * m;
    }
    let mut starts = n1;
    let mut i = 0;
    if can_anchor && pat.first() == Some(&b'^') {
        starts = 1.0;
        i = 1;
    }
    let (mut q, mut rescans) = (0u32, 0i32);
    while i < pat.len() {
        let end = match pat[i] {
            b'(' | b')' => {
                i += 1;
                continue;
            }
            b'%' => match pat.get(i + 1) {
                Some(b'b') => {
                    rescans += 1;
                    i += 4;
                    continue;
                }
                Some(b'f') => {
                    let Some(e) = set_end(pat, i + 2) else {
                        return f64::INFINITY;
                    };
                    i = e;
                    continue;
                }
                Some(d) if d.is_ascii_digit() => {
                    rescans += 1;
                    i += 2;
                    continue;
                }
                _ => i + 2,
            },
            b'[' => match set_end(pat, i) {
                Some(e) => e,
                None => return f64::INFINITY,
            },
            _ => i + 1,
        };
        if matches!(pat.get(end), Some(b'*' | b'+' | b'-' | b'?')) {
            q += 1;
            i = end + 1;
        } else {
            i = end;
        }
    }
    let mut splits = 1.0;
    for k in 1..=q {
        splits = splits * (n as f64 + f64::from(k)) / f64::from(k);
    }
    starts * splits * f64::from(q + 1) * m * n1.powi(rescans)
}

/// Just past the `]` that closes the set opening at `pat[i]` (`classEnd`).
/// `None` is a malformed set, which Lua would reject anyway.
fn set_end(pat: &[u8], i: usize) -> Option<usize> {
    if pat.get(i) != Some(&b'[') {
        return None;
    }
    let mut p = i + 1;
    if pat.get(p) == Some(&b'^') {
        p += 1;
    }
    loop {
        let c = *pat.get(p)?;
        p += 1;
        if c == b'%' {
            p += 1;
        }
        if *pat.get(p)? == b']' {
            return Some(p + 1);
        }
    }
}

/// `pcall` that re-raises once the instruction or time limit has tripped.
/// Lua would otherwise catch the hook error and keep looping.
fn guarded_pcall(lua: &Lua, fuel: &Fuel, args: MultiValue) -> mlua::Result<MultiValue> {
    if fuel.over() {
        return Err(mlua::Error::runtime("lua execution limit exceeded"));
    }
    let mut iter = args.into_vec().into_iter();
    let Some(Value::Function(func)) = iter.next() else {
        return Err(mlua::Error::runtime("pcall expects a function"));
    };
    let rest = MultiValue::from_vec(iter.collect());
    match func.call::<MultiValue>(rest) {
        Ok(vals) => {
            if fuel.over() {
                return Err(mlua::Error::runtime("lua execution limit exceeded"));
            }
            let vals = vals.into_vec();
            let mut out = Vec::with_capacity(1 + vals.len());
            out.push(Value::Boolean(true));
            out.extend(vals);
            Ok(MultiValue::from_vec(out))
        }
        Err(err) => {
            if fuel.over() {
                return Err(mlua::Error::runtime("lua execution limit exceeded"));
            }
            let message = lua.create_string(err.to_string())?;
            Ok(MultiValue::from_vec(vec![
                Value::Boolean(false),
                Value::String(message),
            ]))
        }
    }
}

fn text_only_load(lua: &Lua, args: MultiValue) -> mlua::Result<mlua::Function> {
    let args = args.into_vec();
    let Some(chunk) = args.first() else {
        return Err(mlua::Error::runtime("load expects a string"));
    };
    let bytes = match chunk {
        Value::String(s) => s.as_bytes().to_vec(),
        _ => return Err(mlua::Error::runtime("load expects a string chunk")),
    };
    // Lua bytecode since 5.2 has no verifier. The signature byte is ESC.
    // Mode "b" and "bt" are ignored; the chunk is compiled as text.
    if bytes.first() == Some(&0x1b) {
        return Err(mlua::Error::runtime("binary chunks are not allowed"));
    }
    let name = match args.get(1) {
        Some(Value::String(s)) => s
            .to_str()
            .map(|t| t.to_string())
            .unwrap_or_else(|_| "=(load)".into()),
        _ => "=(load)".to_string(),
    };
    let mut loader = lua.load(bytes).set_mode(ChunkMode::Text).set_name(name);
    if let Some(Value::Table(env)) = args.get(3) {
        loader = loader.set_environment(env.clone());
    }
    let func = loader.into_function()?;
    // `<close>` is compiled into this function, including its nested
    // prototypes. Reject it before the caller can run it.
    if let Err(msg) = reject_to_be_closed(&func) {
        return Err(mlua::Error::runtime(msg));
    }
    Ok(func)
}

fn compile_text(lua: &Lua, source: &str, name: &str) -> Result<mlua::Function, String> {
    let func = lua
        .load(source)
        .set_mode(ChunkMode::Text)
        .set_name(name)
        .into_function()
        .map_err(|e| e.to_string())?;
    reject_to_be_closed(&func)?;
    Ok(func)
}

fn reject_to_be_closed(func: &mlua::Function) -> Result<(), String> {
    let bytes = func.dump(true);
    if bytes.is_empty() {
        return Err("lua chunk could not be checked".into());
    }
    match bytecode_arms_close(&bytes) {
        Ok(false) => Ok(()),
        Ok(true) => Err("<close> is disabled".into()),
        Err(()) => Err("lua chunk could not be checked".into()),
    }
}

/// `OP_TBC` in Lua 5.4. It is the only instruction that marks a local
/// to-be-closed. `__close` then runs while unwinding, with hooks off, so
/// the instruction budget cannot stop a loop inside that handler. The
/// field may be written after `setmetatable`; the opcode is what arms it.
const OP_TBC: u32 = 55;
const LUA_VNIL: u8 = 0;
const LUA_VFALSE: u8 = 1;
const LUA_VTRUE: u8 = 17;
const LUA_VNUMINT: u8 = 3;
const LUA_VNUMFLT: u8 = 19;
const LUA_VSHRSTR: u8 = 4;
const LUA_VLNGSTR: u8 = 20;

/// Whether this official Lua 5.4 dump contains `OP_TBC` in any prototype.
/// `Err` means the bytes are not a dump we can read. Callers reject both.
fn bytecode_arms_close(bytes: &[u8]) -> Result<bool, ()> {
    let mut r = Dump::new(bytes);
    r.header()?;
    let _upvalues = r.byte()?;
    let found = r.function()?;
    if r.i != r.b.len() {
        return Err(());
    }
    Ok(found)
}

struct Dump<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Dump<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }

    fn byte(&mut self) -> Result<u8, ()> {
        let b = *self.b.get(self.i).ok_or(())?;
        self.i += 1;
        Ok(b)
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], ()> {
        let end = self.i.checked_add(n).ok_or(())?;
        let s = self.b.get(self.i..end).ok_or(())?;
        self.i = end;
        Ok(s)
    }

    fn skip(&mut self, n: usize) -> Result<(), ()> {
        let _ = self.bytes(n)?;
        Ok(())
    }

    /// Lua's `dumpSize`: 7-bit groups, most significant first, high bit set
    /// on the last byte.
    fn size(&mut self) -> Result<usize, ()> {
        let mut x: u64 = 0;
        for _ in 0..10 {
            let b = self.byte()?;
            if x > (u64::MAX >> 7) {
                return Err(());
            }
            x = (x << 7) | u64::from(b & 0x7f);
            if b & 0x80 != 0 {
                return usize::try_from(x).map_err(|_| ());
            }
        }
        Err(())
    }

    fn header(&mut self) -> Result<(), ()> {
        if self.bytes(4)? != b"\x1bLua" {
            return Err(());
        }
        if self.byte()? != 0x54 || self.byte()? != 0 {
            return Err(());
        }
        if self.bytes(6)? != b"\x19\x93\r\n\x1a\n" {
            return Err(());
        }
        // Instruction, lua_Integer, lua_Number. Then the endianness checks.
        if self.byte()? != 4 || self.byte()? != 8 || self.byte()? != 8 {
            return Err(());
        }
        if self.bytes(8)? != 0x5678u64.to_le_bytes() {
            return Err(());
        }
        let num = f64::from_le_bytes(self.bytes(8)?.try_into().map_err(|_| ())?);
        if num.to_bits() != 370.5f64.to_bits() {
            return Err(());
        }
        Ok(())
    }

    fn lua_string(&mut self) -> Result<(), ()> {
        let n = self.size()?;
        if n == 0 {
            return Ok(());
        }
        self.skip(n - 1)
    }

    fn function(&mut self) -> Result<bool, ()> {
        self.lua_string()?;
        let _ = self.size()?;
        let _ = self.size()?;
        let _ = self.byte()?;
        let _ = self.byte()?;
        let _ = self.byte()?;
        let mut found = self.code()?;
        self.constants()?;
        self.upvalues()?;
        if self.protos()? {
            found = true;
        }
        self.debug()?;
        Ok(found)
    }

    fn code(&mut self) -> Result<bool, ()> {
        let n = self.bounded()?;
        let mut found = false;
        for _ in 0..n {
            let word = u32::from_le_bytes(self.bytes(4)?.try_into().map_err(|_| ())?);
            if (word & 0x7f) == OP_TBC {
                found = true;
            }
        }
        Ok(found)
    }

    fn constants(&mut self) -> Result<(), ()> {
        let n = self.bounded()?;
        for _ in 0..n {
            match self.byte()? {
                LUA_VNIL | LUA_VFALSE | LUA_VTRUE => {}
                LUA_VNUMINT | LUA_VNUMFLT => self.skip(8)?,
                LUA_VSHRSTR | LUA_VLNGSTR => self.lua_string()?,
                _ => return Err(()),
            }
        }
        Ok(())
    }

    fn upvalues(&mut self) -> Result<(), ()> {
        let n = self.bounded()?;
        self.skip(n.checked_mul(3).ok_or(())?)
    }

    fn protos(&mut self) -> Result<bool, ()> {
        let n = self.bounded()?;
        let mut found = false;
        for _ in 0..n {
            if self.function()? {
                found = true;
            }
        }
        Ok(found)
    }

    fn debug(&mut self) -> Result<(), ()> {
        let lines = self.bounded()?;
        self.skip(lines)?;
        let abs = self.bounded()?;
        for _ in 0..abs {
            let _ = self.size()?;
            let _ = self.size()?;
        }
        let locals = self.bounded()?;
        for _ in 0..locals {
            self.lua_string()?;
            let _ = self.size()?;
            let _ = self.size()?;
        }
        let names = self.bounded()?;
        for _ in 0..names {
            self.lua_string()?;
        }
        Ok(())
    }

    fn bounded(&mut self) -> Result<usize, ()> {
        let n = self.size()?;
        if n > 1_000_000 {
            return Err(());
        }
        Ok(n)
    }
}

fn read_source(path: &Path) -> Result<Vec<u8>, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("refusing a symlink".into());
    }
    if !meta.is_file() {
        return Err("not a file".into());
    }
    if meta.len() > MAX_SOURCE {
        return Err("lua file is too large".into());
    }
    fs::read(path).map_err(|e| e.to_string())
}

fn source_text(bytes: &[u8]) -> Result<&str, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "lua file is not utf-8".to_string())?;
    Ok(text.trim_start_matches('\u{feff}'))
}

fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> Result<Value, String> {
    let v = match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Number(f)
            } else {
                return Err("number does not fit in lua".into());
            }
        }
        serde_json::Value::String(s) => {
            Value::String(lua.create_string(s).map_err(|e| e.to_string())?)
        }
        serde_json::Value::Array(items) => {
            let t = lua.create_table().map_err(|e| e.to_string())?;
            for (i, item) in items.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, item)?)
                    .map_err(|e| e.to_string())?;
            }
            Value::Table(t)
        }
        serde_json::Value::Object(map) => {
            let t = lua.create_table().map_err(|e| e.to_string())?;
            for (k, item) in map {
                t.set(k.as_str(), json_to_lua(lua, item)?)
                    .map_err(|e| e.to_string())?;
            }
            Value::Table(t)
        }
    };
    Ok(v)
}

fn lua_to_json(value: Value) -> Result<serde_json::Value, String> {
    to_json(value, &mut Vec::new(), &mut 0)
}

/// `path` holds the tables between the root and this value; `count` is
/// every value converted so far.
fn to_json(
    value: Value,
    path: &mut Vec<*const std::ffi::c_void>,
    count: &mut usize,
) -> Result<serde_json::Value, String> {
    *count += 1;
    if *count > MAX_CONFIG_VALUES {
        return Err("config has too many values".into());
    }
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(b)),
        Value::Integer(i) => Ok(serde_json::Value::Number(i.into())),
        Value::Number(n) => serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .ok_or_else(|| "non-finite number".into()),
        Value::String(s) => {
            let text = s.to_str().map_err(|_| "lua string is not utf-8")?;
            Ok(serde_json::Value::String(text.to_string()))
        }
        Value::Table(t) => {
            let ptr = t.to_pointer();
            if path.contains(&ptr) {
                return Err("config table contains itself".into());
            }
            if path.len() >= MAX_CONFIG_DEPTH {
                return Err("config tables nest too deep".into());
            }
            path.push(ptr);
            let json = table_to_json(&t, path, count);
            path.pop();
            json
        }
        other => Err(format!("config.lua produced {other:?}")),
    }
}

fn table_to_json(
    t: &mlua::Table,
    path: &mut Vec<*const std::ffi::c_void>,
    count: &mut usize,
) -> Result<serde_json::Value, String> {
    if is_sequence(t)? {
        let n = t.raw_len();
        let mut items = Vec::with_capacity(n);
        for i in 1..=n {
            let v: Value = t.get(i).map_err(|e| e.to_string())?;
            items.push(to_json(v, path, count)?);
        }
        return Ok(serde_json::Value::Array(items));
    }
    let mut map = serde_json::Map::new();
    for pair in t.pairs::<Value, Value>() {
        let (k, v) = pair.map_err(|e| e.to_string())?;
        let Value::String(key) = k else {
            return Err("config table key is not a string".into());
        };
        let key = key.to_str().map_err(|_| "config key is not utf-8")?;
        if matches!(v, Value::Nil) {
            continue;
        }
        map.insert(key.to_string(), to_json(v, path, count)?);
    }
    Ok(serde_json::Value::Object(map))
}

fn is_sequence(t: &mlua::Table) -> Result<bool, String> {
    let len = t.raw_len();
    if len == 0 {
        return Ok(false);
    }
    let mut n = 0usize;
    for pair in t.pairs::<Value, Value>() {
        let (k, _) = pair.map_err(|e| e.to_string())?;
        n += 1;
        match k {
            Value::Integer(i) if i >= 1 && i as usize <= len => {}
            _ => return Ok(false),
        }
    }
    Ok(n == len)
}

fn deep_merge(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(dst), serde_json::Value::Object(src)) => {
            for (k, v) in src {
                if SECURITY_KEYS.contains(&k.as_str()) {
                    continue;
                }
                match dst.get_mut(&k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        deep_merge(existing, v)
                    }
                    Some(existing) => *existing = v,
                    None => {
                        dst.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

fn restore_security(merged: &mut serde_json::Value, original: &serde_json::Value) {
    let Some(obj) = merged.as_object_mut() else {
        return;
    };
    for key in SECURITY_KEYS {
        match original.get(key) {
            Some(v) => {
                obj.insert(key.to_string(), v.clone());
            }
            None => {
                obj.remove(key);
            }
        }
    }
}

fn log_path() -> PathBuf {
    if cfg!(test) {
        std::env::temp_dir().join("bayan-test-lua.log")
    } else {
        crate::config::bayan_dir().join("lua.log")
    }
}

fn log_line(msg: &str) {
    use std::io::Write;
    let p = log_path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}", msg.trim_end());
    }
}

#[cfg(test)]
fn probe(src: &str) -> Result<String, String> {
    probe_with(src, Limits::production())
}

#[cfg(test)]
fn probe_with(src: &str, limits: Limits) -> Result<String, String> {
    let (lua, fuel) = open_sandbox(limits)?;
    fuel.reset();
    let value: Value = compile_text(&lua, src, "probe")?
        .call(())
        .map_err(|e| e.to_string())?;
    match value {
        Value::Nil => Ok(String::new()),
        Value::Boolean(b) => Ok(b.to_string()),
        Value::Integer(i) => Ok(i.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => s.to_str().map(|t| t.to_string()).map_err(|e| e.to_string()),
        other => Ok(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;

    /// Run `f` on another thread. A `__gc` or `__close` loop never returns,
    /// so the assertion has to fail on a timeout instead of wedging CI.
    fn finish_within<T: Send + 'static>(
        limit: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            let _ = tx.send(outcome);
        });
        match rx.recv_timeout(limit) {
            Ok(Ok(value)) => value,
            Ok(Err(payload)) => std::panic::resume_unwind(payload),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("lua did not return within {limit:?}")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("lua worker stopped without a result")
            }
        }
    }

    fn scratch(label: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("bayan-lua-{label}-{n}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn json_cfg(body: &str) -> UserConfig {
        serde_json::from_str(body).unwrap()
    }

    fn approve(cfg: &mut UserConfig, bytes: &[u8]) {
        grant(cfg, &content_hash(bytes));
    }

    #[test]
    fn tests_do_not_write_the_user_lua_log() {
        assert!(!log_path().starts_with(crate::config::bayan_dir()));
    }

    #[test]
    fn sha256_of_empty_is_the_standard_vector() {
        assert_eq!(
            content_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn lua_options_default_catches_rust_panics() {
        // mlua 0.12.2, async feature off: the only field, and it is true.
        let opts = LuaOptions::default();
        assert!(opts.catch_rust_panics);
    }

    #[test]
    fn json_only_config_is_unchanged() {
        let dir = scratch("json-only");
        let cfg =
            json_cfg(r#"{"font_family":"JetBrains Mono","font_size":17.5,"shell":"pwsh.exe"}"#);
        let out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(out.pending.is_empty());
        assert_eq!(
            serde_json::to_value(&out.config).unwrap(),
            serde_json::to_value(&cfg).unwrap()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lua_overrides_merge_over_json() {
        let dir = scratch("merge");
        let src = "config.font_size = 21\nreturn { theme = \"بيان\", font_family = \"كواكب\" }\n";
        fs::write(dir.join("config.lua"), src).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0,"shell":"pwsh.exe"}"#);
        approve(&mut cfg, src.as_bytes());
        let out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.config.font_size, Some(21.0));
        assert_eq!(out.config.font_family.as_deref(), Some("كواكب"));
        assert_eq!(out.config.theme.as_deref(), Some("بيان"));
        assert_eq!(out.config.shell_program(), "pwsh.exe");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lua_syntax_error_falls_back_to_json() {
        let dir = scratch("syntax");
        let src = "function (\n";
        fs::write(dir.join("config.lua"), src).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
        approve(&mut cfg, src.as_bytes());
        let out = load_with(&cfg, &dir, false);
        assert!(!out.errors.is_empty());
        assert_eq!(out.config.font_size, Some(12.0));
        assert!(out.config.theme.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lua_runtime_error_falls_back_to_json() {
        let dir = scratch("runtime");
        let src = "return nil + 1\n";
        fs::write(dir.join("config.lua"), src).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
        approve(&mut cfg, src.as_bytes());
        let out = load_with(&cfg, &dir, false);
        assert!(!out.errors.is_empty(), "a runtime error must be reported");
        assert_eq!(out.config.font_size, Some(12.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unapproved_config_lua_does_not_run() {
        let dir = scratch("unapproved-cfg");
        let src = "config.font_size = 99\n";
        fs::write(dir.join("config.lua"), src).unwrap();
        let cfg = json_cfg(r#"{"font_size":12.0}"#);
        let out = load_with(&cfg, &dir, false);
        assert_eq!(out.config.font_size, Some(12.0));
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.pending[0].hash, content_hash(src.as_bytes()));
        assert!(matches!(out.pending[0].role, ScriptRole::Config));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_does_not_run_a_changed_config_lua() {
        let dir = scratch("reload-cfg");
        let v1 = "config.font_size = 21\n";
        fs::write(dir.join("config.lua"), v1).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
        approve(&mut cfg, v1.as_bytes());
        let first = load_with(&cfg, &dir, false);
        assert_eq!(first.config.font_size, Some(21.0));
        let v2 = "config.font_size = 44\n";
        fs::write(dir.join("config.lua"), v2).unwrap();
        let second = load_with(&cfg, &dir, false);
        assert_eq!(
            second.config.font_size,
            Some(12.0),
            "a changed file keeps the JSON value"
        );
        assert_eq!(second.pending.len(), 1);
        assert_eq!(second.pending[0].hash, content_hash(v2.as_bytes()));
        assert_ne!(second.pending[0].hash, content_hash(v1.as_bytes()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandbox_removes_io_os_debug_package_and_loaders() {
        let present = probe(
            r#"
            local bad = {}
            local function has(name, v)
              if v ~= nil then bad[#bad + 1] = name end
            end
            has("io", io)
            has("os", os)
            has("debug", debug)
            has("package", package)
            has("require", require)
            has("loadfile", loadfile)
            has("dofile", dofile)
            if type(load) ~= "function" then bad[#bad + 1] = "load-missing" end
            return table.concat(bad, ",")
            "#,
        )
        .unwrap();
        assert_eq!(present, "", "forbidden globals: {present}");
        for src in [
            "return os.execute('calc')",
            "return io.open('x')",
            "return package.loadlib('x', 'y')",
            "return require('io')",
            "return loadfile('x')",
            "return dofile('x')",
        ] {
            assert!(probe(src).is_err(), "{src} should fail");
        }
    }

    #[test]
    fn string_dump_then_load_cannot_escape() {
        let dumped = probe(
            r#"
            local f = function() return 123 end
            local ok, dumped = pcall(string.dump, f)
            if not ok then return "dump-blocked" end
            local ok2, fn = pcall(load, dumped, "dumped", "b")
            if ok2 and type(fn) == "function" then return "escaped" end
            return "load-blocked"
            "#,
        )
        .unwrap();
        assert_eq!(dumped, "dump-blocked");
        let binary = probe(
            r#"
            local ok, fn = pcall(load, "\27Lua", "evil", "b")
            if ok and type(fn) == "function" then return "escaped" end
            ok, fn = pcall(load, "\27Lua", "evil", "bt")
            if ok and type(fn) == "function" then return "escaped" end
            return "load-blocked"
            "#,
        )
        .unwrap();
        assert_eq!(binary, "load-blocked");
    }

    #[test]
    fn an_infinite_loop_is_cut_off() {
        finish_within(Duration::from_secs(5), || {
            let start = Instant::now();
            let err = probe("while true do end").unwrap_err();
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "loop ran for {:?}",
                start.elapsed()
            );
            assert!(
                err.contains("limit"),
                "expected the execution limit, got {err}"
            );
        });
    }

    #[test]
    fn pcall_cannot_swallow_the_execution_limit() {
        finish_within(Duration::from_secs(5), || {
            let start = Instant::now();
            let err = probe(
                r#"
            pcall(function() while true do end end)
            return "continued"
            "#,
            )
            .unwrap_err();
            assert!(start.elapsed() < Duration::from_secs(3));
            assert!(err.contains("limit"), "{err}");
        });
    }

    #[test]
    fn the_memory_limit_rejects_a_huge_string() {
        let (lua, _) = open_sandbox(Limits {
            memory: 32 * 1024 * 1024,
            ..Limits::production()
        })
        .unwrap();
        let used = lua.used_memory();
        lua.set_memory_limit(used.saturating_add(256 * 1024))
            .unwrap();
        let small: i64 = lua
            .load("return #string.rep('x', 100)")
            .set_mode(ChunkMode::Text)
            .eval()
            .unwrap();
        assert_eq!(small, 100);
        let huge = lua
            .load("return string.rep('x', 2 * 1024 * 1024)")
            .set_mode(ChunkMode::Text)
            .eval::<Value>();
        assert!(huge.is_err(), "a 2 MiB string must trip the memory limit");
    }

    #[test]
    fn arabic_strings_round_trip() {
        let (lua, _) = open_sandbox(Limits::production()).unwrap();
        let s = "السلام عليكم — بيان";
        lua.globals().set("s", s).unwrap();
        let back: String = lua.globals().get("s").unwrap();
        assert_eq!(back, s);
        let joined: String = lua
            .load("return s .. ' · ' .. 'عربي'")
            .set_mode(ChunkMode::Text)
            .eval()
            .unwrap();
        assert_eq!(joined, "السلام عليكم — بيان · عربي");
    }

    #[test]
    fn config_lua_cannot_grant_spawn_or_see_bayan() {
        let dir = scratch("no-spawn");
        let src = r#"
            if bayan ~= nil then error("bayan leaked into config.lua") end
            config.lua_plugins = { { name = "x", spawn = true } }
            config.lua_plugins_dir = "C:/evil"
            config.font_size = 19
        "#;
        fs::write(dir.join("config.lua"), src).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
        approve(&mut cfg, src.as_bytes());
        let out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.config.font_size, Some(19.0));
        assert!(out.config.lua_plugins.is_empty());
        assert!(out.config.lua_plugins_dir.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn executable_plugins_are_disabled_without_an_allowlist() {
        let dir = scratch("disabled");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let src = "bayan.bind('ctrl+alt+g', function() bayan.send('ran') end)\n";
        fs::write(plugins.join("hello.lua"), src).unwrap();
        let mut cfg = json_cfg(&format!(
            r#"{{"lua_plugins_dir":"{}"}}"#,
            plugins.display().to_string().replace('\\', "\\\\")
        ));
        approve(&mut cfg, src.as_bytes());
        let mut out = load_with(&cfg, &dir, false);
        assert!(
            out.pending.is_empty(),
            "an unlisted plugin is not even offered"
        );
        assert!(!out.plugins.has_binding("ctrl+alt+g"));
        assert!(out.plugins.fire("ctrl+alt+g").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unapproved_plugin_does_not_run() {
        let dir = scratch("unapproved-plugin");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let src =
            "bayan.send('pwned')\nbayan.bind('ctrl+alt+g', function() bayan.send('ran') end)\n";
        fs::write(plugins.join("hello.lua"), src).unwrap();
        let cfg = plugin_cfg(&plugins, "hello", false);
        let mut out = load_with(&cfg, &dir, false);
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.plugins.callback_runs(), 0);
        assert!(out.plugins.fire("ctrl+alt+g").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_allowlisted_plugin_keybinding_fires() {
        let dir = scratch("fires");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let src = "bayan.bind('ctrl+alt+g', function() bayan.send('git status\\r') end)\n";
        fs::write(plugins.join("hello.lua"), src).unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", false);
        approve(&mut cfg, src.as_bytes());
        let mut out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(out.pending.is_empty());
        let sends = out.plugins.fire("ctrl+alt+g");
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].plugin, "hello");
        assert_eq!(sends[0].text, "git status\r");
        assert_eq!(out.plugins.callback_runs(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_plugin_needs_reapproval() {
        let dir = scratch("changed-plugin");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let v1 = "bayan.bind('ctrl+alt+g', function() bayan.send('one') end)\n";
        fs::write(plugins.join("hello.lua"), v1).unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", false);
        approve(&mut cfg, v1.as_bytes());
        let mut first = load_with(&cfg, &dir, false);
        assert_eq!(first.plugins.fire("ctrl+alt+g")[0].text, "one");
        let v2 = "bayan.bind('ctrl+alt+h', function() bayan.send('two') end)\n";
        fs::write(plugins.join("hello.lua"), v2).unwrap();
        let mut second = load_with(&cfg, &dir, false);
        assert!(
            second.plugins.fire("ctrl+alt+h").is_empty(),
            "the new file must not run on the old hash"
        );
        assert!(second.plugins.fire("ctrl+alt+g").is_empty());
        assert_eq!(second.pending.len(), 1);
        assert_eq!(second.pending[0].hash, content_hash(v2.as_bytes()));
        assert!(unchanged(&second.pending[0]).is_ok());
        fs::write(plugins.join("hello.lua"), "bayan.send('nope')\n").unwrap();
        assert!(unchanged(&second.pending[0]).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_is_denied_without_the_flag_and_absent_from_config_lua() {
        let dir = scratch("spawn");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let ask = r#"
            bayan.bind("f12", function()
              if bayan.spawn == nil then
                bayan.send("nil")
                return
              end
              local ok = pcall(bayan.spawn, "bayan-no-such-program", {})
              if ok then bayan.send("spawned") else bayan.send("failed") end
            end)
        "#;
        fs::write(plugins.join("hello.lua"), ask).unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", false);
        approve(&mut cfg, ask.as_bytes());
        let mut out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.plugins.fire("f12")[0].text, "nil");

        let mut cfg_on = plugin_cfg(&plugins, "hello", true);
        approve(&mut cfg_on, ask.as_bytes());
        let mut on = load_with(&cfg_on, &dir, false);
        assert!(on.errors.is_empty(), "{:?}", on.errors);
        assert_eq!(
            on.plugins.fire("f12")[0].text,
            "failed",
            "the flag exposes spawn, and a missing program is an error"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugins_load_only_from_the_configured_directory() {
        let dir = scratch("path");
        let plugins = dir.join("plugins");
        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(&plugins).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        let src = "bayan.bind('f12', function() bayan.send('here') end)\n";
        fs::write(elsewhere.join("hello.lua"), src).unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", false);
        approve(&mut cfg, src.as_bytes());
        let mut out = load_with(&cfg, &dir, false);
        assert!(out.plugins.fire("f12").is_empty());
        assert!(
            !out.errors.is_empty(),
            "a missing file in the configured dir is an error"
        );

        cfg.lua_plugins[0].name = "../elsewhere/hello".into();
        let mut escaped = load_with(&cfg, &dir, false);
        assert!(escaped.plugins.fire("f12").is_empty());
        assert!(
            escaped.errors.iter().any(|e| e.contains("file stem")),
            "{:?}",
            escaped.errors
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plugin_event_handler_runs_only_when_the_host_emits_it() {
        let dir = scratch("event");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let src = r#"
            bayan.on("tab-opened", function(ev)
              bayan.send(ev.event .. " " .. ev.index)
            end)
            bayan.on("bell", function() bayan.send("bell") end)
            bayan.on("title-changed", function() bayan.send("title") end)
            bayan.on("osc7-cwd", function(ev) bayan.send(ev.cwd) end)
        "#;
        fs::write(plugins.join("hello.lua"), src).unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", false);
        approve(&mut cfg, src.as_bytes());
        let mut out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.plugins.callback_runs(), 0);
        let sends = out.plugins.emit("tab-opened", &[("index", "2")]);
        assert_eq!(sends[0].text, "tab-opened 2");
        let arabic = out.plugins.emit("osc7-cwd", &[("cwd", "مجلد")]);
        assert_eq!(arabic[0].text, "مجلد");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_output_never_runs_lua() {
        let dir = scratch("pty");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let src = r#"
            local function flag(ev) bayan.send("ran:" .. ev.event) end
            bayan.on("tab-opened", flag)
            bayan.on("bell", flag)
            bayan.on("title-changed", flag)
            bayan.on("osc7-cwd", function(ev) bayan.send("ran:" .. (ev.cwd or "")) end)
            bayan.bind("ctrl+alt+g", function() bayan.send("bound") end)
        "#;
        fs::write(plugins.join("hello.lua"), src).unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", false);
        approve(&mut cfg, src.as_bytes());
        let mut out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let runs = out.plugins.callback_runs();
        let samples: &[&[u8]] = &[
            b"\x1b]7;file://host/C:/Users/someone\x07",
            b"\x1b]0;\xd8\xa8\xd9\x8a\xd8\xa7\xd9\x86\x07",
            b"\x1b]8;;https://example.com/a\x07click\x1b]8;;\x07",
            b"\x07",
            b"hello from the program\r\n",
            b"\x1b[31mred\x1b[0m",
            b"\x1b]777;notify;T;body\x07",
            "echo مرحبا\r\n".as_bytes(),
        ];
        for sample in samples {
            feed_terminal(sample);
            let facts = PtyFacts {
                bell: sample.contains(&0x07),
                title: Some("بيان".into()),
                cwd: Some("C:/Users/someone".into()),
                hyperlink: sample.windows(6).any(|w| w == b"\x1b]8;;"),
                output: true,
            };
            // The same function the bell and cwd handlers call. If it starts
            // dispatching, this plugin's handlers send and the test fails.
            assert!(
                plugin_sends_from_pty(&out.plugins, &facts).is_empty(),
                "terminal facts must not run Lua"
            );
        }
        assert_eq!(out.plugins.callback_runs(), runs);
        assert_eq!(out.plugins.fire("ctrl+alt+g").len(), 1);
        // The binding still works, so the silence was the sequences.
        assert!(out
            .plugins
            .emit("bell", &[])
            .iter()
            .any(|s| s.text == "ran:bell"));
        let after = out.plugins.callback_runs();
        let again = plugin_sends_from_pty(
            &out.plugins,
            &PtyFacts {
                bell: true,
                title: Some("x".into()),
                cwd: Some("y".into()),
                hyperlink: true,
                output: true,
            },
        );
        assert!(again.is_empty());
        assert_eq!(out.plugins.callback_runs(), after);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn elevated_startup_runs_no_lua_and_loads_no_plugins() {
        // Files are present and already approved. The injected flag is what
        // `is_elevated` returns for a UAC token or Explorer's Run as
        // administrator. `load_at` and `reload` pass that function through.
        let dir = scratch("elevated");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let lua_src = "config.font_size = 99\n";
        fs::write(dir.join("config.lua"), lua_src).unwrap();
        let plug = "bayan.bind('ctrl+alt+g', function() bayan.send('ran') end)\n";
        fs::write(plugins.join("hello.lua"), plug).unwrap();
        fs::write(
            plugins.join("theme.json"),
            r##"{"themes":[{"name":"Mine","bg":"#101010","fg":"#ffffff","ansi":{}}]}"##,
        )
        .unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", false);
        cfg.font_size = Some(12.0);
        approve(&mut cfg, lua_src.as_bytes());
        approve(&mut cfg, plug.as_bytes());

        let mut elevated = load_with(&cfg, &dir, true);
        assert_eq!(elevated.config.font_size, Some(12.0), "the JSON value");
        assert!(elevated.pending.is_empty(), "no confirm card, no later run");
        assert!(elevated.errors.is_empty());
        assert_eq!(elevated.notice.as_deref(), Some(ELEVATED_NOTICE));
        assert!(
            ELEVATED_NOTICE.chars().any(char::is_alphabetic),
            "the notice is the Arabic sentence"
        );
        assert!(ELEVATED_NOTICE.contains("مسؤول"));
        assert_eq!(elevated.plugins.callback_runs(), 0);
        assert!(!elevated.plugins.has_binding("ctrl+alt+g"));
        assert!(elevated.plugins.fire("ctrl+alt+g").is_empty());
        assert!(!scripts_enabled(true));
        let skipped = if scripts_enabled(true) {
            crate::plugins::load_from(&plugins, &[])
        } else {
            crate::plugins::Registry::default()
        };
        assert!(skipped.loaded.is_empty() && skipped.themes.is_empty());

        // The same files still run when the token is not elevated.
        assert!(scripts_enabled(false));
        let mut open = load_with(&cfg, &dir, false);
        assert!(open.notice.is_none());
        assert!(open.errors.is_empty(), "{:?}", open.errors);
        assert_eq!(open.config.font_size, Some(99.0));
        assert_eq!(open.plugins.fire("ctrl+alt+g")[0].text, "ran");
        let loaded = crate::plugins::load_from(&plugins, &[]);
        assert_eq!(loaded.loaded, vec!["theme".to_string()]);
        // The live token, whatever this process is. Scripts follow it.
        let live = is_elevated();
        assert_eq!(scripts_enabled(live), !live);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_callback_still_runs_after_the_deadline() {
        let dir = scratch("budget");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let src = "bayan.bind('f12', function() local s=0; for i=1,5000 do s=s+i end; bayan.send('ok') end)\n";
        fs::write(plugins.join("hello.lua"), src).unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", false);
        approve(&mut cfg, src.as_bytes());
        let mut out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        std::thread::sleep(Duration::from_millis(150));
        let sends = out.plugins.fire("f12");
        assert_eq!(sends.len(), 1, "the budget must reset for each call");
        assert_eq!(sends[0].text, "ok");
        for _ in 0..50 {
            assert_eq!(out.plugins.fire("f12")[0].text, "ok");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_runaway_callback_is_cut_off_and_the_next_call_works() {
        finish_within(Duration::from_secs(5), || {
            let dir = scratch("runaway");
            let plugins = dir.join("plugins");
            fs::create_dir_all(&plugins).unwrap();
            let src = r#"
            bayan.bind("f12", function() while true do end end)
            bayan.bind("f11", function() local s=0; for i=1,5000 do s=s+i end; bayan.send("ok") end)
        "#;
            fs::write(plugins.join("hello.lua"), src).unwrap();
            let mut cfg = plugin_cfg(&plugins, "hello", false);
            approve(&mut cfg, src.as_bytes());
            let mut out = load_with(&cfg, &dir, false);
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            let start = Instant::now();
            assert!(out.plugins.fire("f12").is_empty());
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "a loop must still be killed"
            );
            assert_eq!(out.plugins.fire("f11")[0].text, "ok");
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn lua_values_are_not_what_gets_saved() {
        let dir = scratch("persist");
        let src = "config.font_size = 99\n";
        fs::write(dir.join("config.lua"), src).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
        approve(&mut cfg, src.as_bytes());
        let out = load_with(&cfg, &dir, false);
        assert_eq!(out.config.font_size, Some(99.0));
        assert_eq!(out.persisted.font_size, Some(12.0));
        let saved = serde_json::to_string(&out.persisted).unwrap();
        assert!(
            !saved.contains("\"font_size\":99"),
            "lua's font size must not be the saved document: {saved}"
        );
        fs::remove_file(dir.join("config.lua")).unwrap();
        let again = load_with(&out.persisted, &dir, false);
        assert_eq!(again.config.font_size, Some(12.0));
        assert!(again.notice.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_script_is_not_approved_until_the_card_shows_it_all() {
        let body = format!("-- {}\nconfig.font_size = 12\n", "x".repeat(2000));
        let script = pending_script(
            ScriptRole::Config,
            Path::new("config.lua"),
            &body,
            content_hash(body.as_bytes()),
        );
        assert!(!script.shows_everything());
        assert!(script.preview.contains(&script.hash));
        assert!(script.preview.contains("bytes"));
        assert!(!script.preview.contains(&"x".repeat(2000)));
        let revealed = script.reveal();
        assert!(revealed.shows_everything());
        assert!(revealed.preview.contains(&"x".repeat(2000)));
    }

    #[test]
    fn a_script_error_shown_to_the_user_is_arabic() {
        let dir = scratch("arabic-err");
        let src = "function (\n";
        fs::write(dir.join("config.lua"), src).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
        approve(&mut cfg, src.as_bytes());
        let out = load_with(&cfg, &dir, false);
        assert!(!out.errors.is_empty());
        assert!(!out.shown.is_empty());
        assert!(
            out.shown[0]
                .chars()
                .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
            "the window text is Arabic: {}",
            out.shown[0]
        );
        assert!(
            !out.shown[0].contains("function"),
            "the window text stays Arabic: {}",
            out.shown[0]
        );
        assert!(
            out.errors[0].contains("syntax") || out.errors[0].contains("function"),
            "the log keeps the technical error: {}",
            out.errors[0]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_elevated_window_without_scripts_stays_quiet() {
        let dir = scratch("quiet");
        let cfg = json_cfg(r#"{"font_size":12.0}"#);
        let out = load_with(&cfg, &dir, true);
        assert!(out.notice.is_none(), "{:?}", out.notice);
        assert_eq!(out.config.font_size, Some(12.0));
        assert_eq!(out.plugins.callback_runs(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn spawn_returns_without_waiting_for_the_process() {
        let dir = scratch("spawn-fast");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let src = r#"
            bayan.bind("f12", function()
              local ok = pcall(bayan.spawn, "ping.exe", {"-n", "15", "127.0.0.1"})
              if ok then bayan.send("fast") else bayan.send("err") end
            end)
        "#;
        fs::write(plugins.join("hello.lua"), src).unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", true);
        approve(&mut cfg, src.as_bytes());
        let mut out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let start = Instant::now();
        let sends = out.plugins.fire("f12");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "spawn blocked the caller for {:?}",
            start.elapsed()
        );
        assert_eq!(sends[0].text, "fast");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_finalizer_loop_fails_fast_instead_of_hanging() {
        finish_within(Duration::from_secs(5), || {
            let dir = scratch("gc");
            let src =
                "setmetatable({}, {__gc=function() while true do end end})\nconfig.font_size = 3\n";
            fs::write(dir.join("config.lua"), src).unwrap();
            let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
            approve(&mut cfg, src.as_bytes());
            let start = Instant::now();
            {
                let out = load_with(&cfg, &dir, false);
                assert!(!out.errors.is_empty(), "a __gc metatable must be rejected");
                assert_ne!(out.config.font_size, Some(3.0));
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "dropping the state hung in a finalizer"
            );

            let plugins = dir.join("plugins");
            fs::create_dir_all(&plugins).unwrap();
            let plug = "setmetatable({}, {__gc=function() while true do end end})\n";
            fs::write(plugins.join("hello.lua"), plug).unwrap();
            let mut pcfg = plugin_cfg(&plugins, "hello", false);
            approve(&mut pcfg, plug.as_bytes());
            let start = Instant::now();
            {
                let out = load_with(&pcfg, &dir, false);
                assert!(!out.errors.is_empty(), "{:?}", out.errors);
                assert_eq!(out.plugins.callback_runs(), 0);
            }
            assert!(start.elapsed() < Duration::from_secs(2));

            let raw = r#"
            local t, mt = {}, {}
            setmetatable(t, mt)
            local ok = pcall(function() rawset(mt, "__gc", function() while true do end end) end)
            if ok then error("rawset allowed __gc") end
            config.font_size = 7
        "#;
            fs::write(dir.join("config.lua"), raw).unwrap();
            let mut rcfg = json_cfg(r#"{"font_size":12.0}"#);
            approve(&mut rcfg, raw.as_bytes());
            let out = load_with(&rcfg, &dir, false);
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            assert_eq!(out.config.font_size, Some(7.0));
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn an_event_keeps_thirty_two_handlers_and_one_deadline() {
        finish_within(Duration::from_secs(5), || {
            let dir = scratch("handlers");
            let plugins = dir.join("plugins");
            fs::create_dir_all(&plugins).unwrap();
            let src = r#"
            local n = 0
            for i = 1, 1000 do
              local ok = pcall(function()
                bayan.on("bell", function() while true do end end)
              end)
              if ok then n = n + 1 end
            end
            bayan.bind("f12", function() bayan.send(tostring(n)) end)
        "#;
            fs::write(plugins.join("hello.lua"), src).unwrap();
            let mut cfg = plugin_cfg(&plugins, "hello", false);
            approve(&mut cfg, src.as_bytes());
            let mut out = load_with(&cfg, &dir, false);
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            assert_eq!(out.plugins.fire("f12")[0].text, "32");
            let before = out.plugins.callback_runs();
            let start = Instant::now();
            let _ = out.plugins.emit("bell", &[]);
            assert!(
                start.elapsed() < Duration::from_millis(500),
                "handlers must share one deadline, took {:?}",
                start.elapsed()
            );
            // The first spinning handler spends the shared budget. The rest
            // must not be entered, so the run count moves by one, not by 32.
            assert_eq!(out.plugins.callback_runs() - before, 1);
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn a_backtracking_pattern_is_rejected() {
        let dir = scratch("pattern");
        let src = r#"
            local ok = pcall(string.find, string.rep("a", 24), ".-.-.-.-.-.-.-.-b")
            if ok then error("pattern ran") end
            config.font_size = 15
        "#;
        fs::write(dir.join("config.lua"), src).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
        approve(&mut cfg, src.as_bytes());
        let start = Instant::now();
        let out = load_with(&cfg, &dir, false);
        assert!(
            start.elapsed() < Duration::from_millis(400),
            "the pattern ran for {:?}",
            start.elapsed()
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.config.font_size, Some(15.0));
        let _ = fs::remove_dir_all(&dir);
    }

    /// config.lua errors here; it must not take the process down.
    fn config_lua_fails(src: &str) -> String {
        let dir = scratch("tables");
        fs::write(dir.join("config.lua"), src).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
        approve(&mut cfg, src.as_bytes());
        let out = load_with(&cfg, &dir, false);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(out.config.font_size, Some(12.0), "{src}");
        assert_eq!(out.errors.len(), 1, "{src}: {:?}", out.errors);
        out.errors[0].clone()
    }

    #[test]
    fn a_config_table_that_contains_itself_is_an_error() {
        let err = config_lua_fails("local t = {} t.x = t config.foo = t");
        assert!(err.contains("contains itself"), "{err}");
        let err = config_lua_fails("local t = {} t[1] = t return { foo = t }");
        assert!(err.contains("contains itself"), "{err}");
    }

    #[test]
    fn deeply_nested_config_tables_are_an_error() {
        let err =
            config_lua_fails("local t = {} for i = 1, 100000 do t = { t } end config.foo = t");
        assert!(err.contains("nest too deep"), "{err}");
    }

    #[test]
    fn shared_config_subtables_cannot_multiply() {
        // 2^20 leaves from 21 tables: the walk would build every copy.
        let err = config_lua_fails(
            "local t = {} for i = 1, 20 do t = { a = t, b = t } end config.foo = t",
        );
        assert!(err.contains("too many values"), "{err}");
    }

    #[test]
    fn a_pattern_too_costly_for_its_subject_is_refused_before_it_runs() {
        finish_within(Duration::from_secs(5), || {
            // Short pattern, no dots, subject under 1 KiB: n^4 steps in one C call.
            for src in [
                r#"return select(2, pcall(function() return ("a"):rep(1000):find("a*a*a*b") end))"#,
                r#"return select(2, pcall(string.match, ("a"):rep(1000), "%a-%a-%a-b"))"#,
                r#"return select(2, pcall(function() return ("a"):rep(1e5):find(".-.-.-.-.-b") end))"#,
                r#"return select(2, pcall(function() for _ in ("a"):rep(600):gmatch("(a*)(a*)b") do end end))"#,
                r#"return select(2, pcall(string.gsub, ("a"):rep(600), "a+a+b", ""))"#,
                r#"return select(2, pcall(string.find, ("a"):rep(1e6), ("a"):rep(1e5) .. "b", 1, true))"#,
            ] {
                let err = probe(src).unwrap();
                assert!(err.contains("too costly"), "{src}: {err}");
            }
        });
    }

    #[test]
    fn a_loop_of_admitted_patterns_stops_at_the_deadline() {
        finish_within(Duration::from_secs(5), || {
            // Each call is under the budget (a few ms), and ~200 of them fit
            // between two hook runs. The call itself must check the deadline.
            let src = r#"
            local s = ("a"):rep(1200)
            for i = 1, 1e9 do s:find("a*b") end
        "#;
            let start = Instant::now();
            let err = probe(src).unwrap_err();
            let took = start.elapsed();
            assert!(err.contains("limit exceeded"), "{err}");
            assert!(took < Duration::from_millis(400), "took {took:?}");
        });
    }

    #[test]
    fn ordinary_patterns_still_match() {
        assert_eq!(
            probe(r#"return ("key=value"):match("(%w+)=(%w+)")"#).unwrap(),
            "key"
        );
        assert_eq!(
            probe(r#"return (("  pad me  "):rep(8)):match("^%s*(.-)%s*$"):sub(1, 6)"#).unwrap(),
            "pad me"
        );
        let path = format!(
            "return (\"C:\\\\{}\"):gsub(\"\\\\\", \"/\")",
            "x".repeat(4000)
        );
        assert!(probe(&path).unwrap().starts_with("C:/x"));
        assert_eq!(
            probe(r#"return select(2, ("a b c"):gsub("%s+", "_"))"#).unwrap(),
            "2"
        );
        assert_eq!(probe(r#"return ("[x]"):find("[]]")"#).unwrap(), "3");
    }

    #[test]
    fn rep_and_move_cannot_loop_in_c() {
        // Zero bytes copied n times: the memory limit never trips.
        assert_eq!(
            probe(r#"return #string.rep("", math.maxinteger)"#).unwrap(),
            "0"
        );
        assert_eq!(
            probe(r#"return #string.rep("", math.maxinteger, "")"#).unwrap(),
            "0"
        );
        let err = probe(r#"return select(2, pcall(string.rep, "ab", 1e7))"#).unwrap();
        assert!(err.contains("too long"), "{err}");
        assert_eq!(probe(r#"return #("ab"):rep(3, ",")"#).unwrap(), "8");
        // Moving nils into a plain table allocates nothing either.
        let err =
            probe(r#"return select(2, pcall(table.move, {}, 1, math.maxinteger - 1, 1, {}))"#)
                .unwrap();
        assert!(err.contains("too many elements"), "{err}");
        assert_eq!(
            probe(r#"return table.concat(table.move({1, 2, 3}, 1, 3, 2), ",")"#).unwrap(),
            "1,1,2,3"
        );
    }

    #[test]
    fn a_fifth_spawn_is_refused_while_four_are_running() {
        let slot = AtomicUsize::new(0);
        assert!(try_begin_spawn(&slot));
        assert!(try_begin_spawn(&slot));
        assert!(try_begin_spawn(&slot));
        assert!(try_begin_spawn(&slot));
        assert!(!try_begin_spawn(&slot), "the cap is four");
        end_spawn(&slot);
        assert!(try_begin_spawn(&slot));
        while slot.load(Ordering::Relaxed) > 0 {
            end_spawn(&slot);
        }
    }

    #[test]
    fn bytecode_scanner_sees_close_in_nested_functions_not_in_strings() {
        let (lua, _) = open_sandbox(Limits::production()).unwrap();
        let nested = compile_text(
            &lua,
            "local function f()\n  local x <close> = nil\nend\nreturn 1\n",
            "nested",
        );
        assert!(
            nested.is_err(),
            "a nested <close> must be refused before it runs"
        );
        let plain = lua
            .load("return 'local x <close> = 1\\55'")
            .set_mode(ChunkMode::Text)
            .into_function()
            .unwrap();
        assert_eq!(bytecode_arms_close(&plain.dump(true)), Ok(false));
        let with_upvalue = lua
            .load("local a = 'hello'\nlocal function f()\n  return a .. 'local x <close>'\nend\nreturn 1\n")
            .set_mode(ChunkMode::Text)
            .into_function()
            .unwrap();
        assert_eq!(bytecode_arms_close(&with_upvalue.dump(true)), Ok(false));
        let inner = lua
            .load("local function f()\n  local x <close> = nil\nend\n")
            .set_mode(ChunkMode::Text)
            .into_function()
            .unwrap();
        assert_eq!(bytecode_arms_close(&inner.dump(true)), Ok(true));
    }

    #[test]
    fn a_close_variable_cannot_run_a_loop_after_the_budget() {
        finish_within(Duration::from_secs(5), || {
            let dir = scratch("close");
            let src = "local x <close> = setmetatable({}, {__close=function() while true do end end})\nwhile true do end\nconfig.font_size = 4\n";
            fs::write(dir.join("config.lua"), src).unwrap();
            let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
            approve(&mut cfg, src.as_bytes());
            let out = load_with(&cfg, &dir, false);
            assert!(
                out.errors.iter().any(|e| e.contains("<close>")),
                "{:?}",
                out.errors
            );
            assert_ne!(out.config.font_size, Some(4.0));

            let plugins = dir.join("plugins");
            fs::create_dir_all(&plugins).unwrap();
            let plug = r#"
                bayan.bind("f12", function()
                  local x <close> = setmetatable({}, {__close=function() while true do end end})
                  while true do end
                  bayan.send("ran")
                end)
            "#;
            fs::write(plugins.join("hello.lua"), plug).unwrap();
            let mut pcfg = plugin_cfg(&plugins, "hello", false);
            approve(&mut pcfg, plug.as_bytes());
            let mut out = load_with(&pcfg, &dir, false);
            assert!(
                out.errors.iter().any(|e| e.contains("<close>")),
                "{:?}",
                out.errors
            );
            assert!(out.plugins.fire("f12").is_empty());
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn a_close_handler_assigned_after_setmetatable_is_rejected() {
        finish_within(Duration::from_secs(5), || {
            let dir = scratch("late-close");
            let src = r#"
                local mt = {}
                local o = setmetatable({}, mt)
                mt.__close = function() while true do end end
                local x <close> = o
                while true do end
                config.font_size = 4
            "#;
            fs::write(dir.join("config.lua"), src).unwrap();
            let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
            approve(&mut cfg, src.as_bytes());
            let out = load_with(&cfg, &dir, false);
            assert!(
                out.errors.iter().any(|e| e.contains("<close>")),
                "{:?}",
                out.errors
            );
            assert_ne!(out.config.font_size, Some(4.0));

            let plugins = dir.join("plugins");
            fs::create_dir_all(&plugins).unwrap();
            let plug = r#"
                local function arm()
                  local mt = {}
                  local o = setmetatable({}, mt)
                  mt.__close = function() while true do end end
                  local ok, f = pcall(load, "local x <close> = ...\nwhile true do end\n")
                  if ok and f then f(o) end
                end
                bayan.bind("f12", function()
                  arm()
                  bayan.send("ran")
                end)
            "#;
            fs::write(plugins.join("hello.lua"), plug).unwrap();
            let mut pcfg = plugin_cfg(&plugins, "hello", false);
            approve(&mut pcfg, plug.as_bytes());
            let mut out = load_with(&pcfg, &dir, false);
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            let sent = out.plugins.fire("f12");
            assert_eq!(sent.len(), 1, "load of <close> must fail before the loop");
            assert_eq!(sent[0].text, "ran");
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn a_generic_for_cannot_run_a_close_handler() {
        finish_within(Duration::from_secs(5), || {
            let dir = scratch("tfor");
            let src = r#"
                local ok, obj = pcall(setmetatable, {}, {__close = function() while true do end end})
                if ok then
                  for _ in next, {}, nil, obj do end
                end
                local sm = getmetatable("")
                pcall(function() rawset(sm, "__close", function() while true do end end) end)
                pcall(function() sm.__close = function() while true do end end end)
                local closed = pcall(function() for _ in next, {}, nil, "z" do end end)
                if closed then error("a string was closable") end
                config.font_size = 4
            "#;
            fs::write(dir.join("config.lua"), src).unwrap();
            let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
            approve(&mut cfg, src.as_bytes());
            let out = load_with(&cfg, &dir, false);
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            assert_eq!(out.config.font_size, Some(4.0));

            let plugins = dir.join("plugins");
            fs::create_dir_all(&plugins).unwrap();
            let plug = r#"
                bayan.bind("f12", function()
                  local ok, obj = pcall(setmetatable, {}, {__close = function() while true do end end})
                  if ok then
                    for _ in next, {}, nil, obj do end
                  end
                  local sm = getmetatable("")
                  pcall(function() rawset(sm, "__close", function() while true do end end) end)
                  local closed = pcall(function() for _ in next, {}, nil, "z" do end end)
                  if closed then error("a string was closable") end
                  bayan.send("ok")
                end)
            "#;
            fs::write(plugins.join("hello.lua"), plug).unwrap();
            let mut pcfg = plugin_cfg(&plugins, "hello", false);
            approve(&mut pcfg, plug.as_bytes());
            let mut out = load_with(&pcfg, &dir, false);
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            assert_eq!(out.plugins.fire("f12")[0].text, "ok");
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn close_added_through_getmetatable_does_not_run() {
        finish_within(Duration::from_secs(5), || {
            let dir = scratch("gmt");
            let src = r#"
                local o = setmetatable({}, {__index = {a = 1}})
                local g = getmetatable(o)
                pcall(function() g.__close = function() while true do end end end)
                pcall(function() rawset(g, "__close", function() while true do end end) end)
                local closed = pcall(function() for _ in next, {}, nil, o do end end)
                if closed then error("the view armed __close") end
                if rawget(g, "__close") ~= nil then error("rawget saw __close") end
                if o.a ~= 1 then error("index") end
                config.font_size = 5
            "#;
            fs::write(dir.join("config.lua"), src).unwrap();
            let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
            approve(&mut cfg, src.as_bytes());
            let out = load_with(&cfg, &dir, false);
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            assert_eq!(out.config.font_size, Some(5.0));
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn close_added_to_the_callers_metatable_does_not_run() {
        finish_within(Duration::from_secs(5), || {
            let dir = scratch("cmt");
            let src = r#"
                local mt = {__index = {a = 1}}
                local o = setmetatable({}, mt)
                mt.__close = function() while true do end end
                rawset(mt, "__close", function() while true do end end)
                local closed = pcall(function() for _ in next, {}, nil, o do end end)
                if closed then error("the caller metatable armed __close") end
                if o.a ~= 1 then error("index") end
                config.font_size = 6
            "#;
            fs::write(dir.join("config.lua"), src).unwrap();
            let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
            approve(&mut cfg, src.as_bytes());
            let out = load_with(&cfg, &dir, false);
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            assert_eq!(out.config.font_size, Some(6.0));

            let plugins = dir.join("plugins");
            fs::create_dir_all(&plugins).unwrap();
            let plug = r#"
                bayan.bind("f12", function()
                  local mt = {}
                  local o = setmetatable({}, mt)
                  mt.__close = function() while true do end end
                  local closed = pcall(function() for _ in next, {}, nil, o do end end)
                  if closed then error("the caller metatable armed __close") end
                  bayan.send("ok")
                end)
            "#;
            fs::write(plugins.join("hello.lua"), plug).unwrap();
            let mut pcfg = plugin_cfg(&plugins, "hello", false);
            approve(&mut pcfg, plug.as_bytes());
            let mut out = load_with(&pcfg, &dir, false);
            assert!(out.errors.is_empty(), "{:?}", out.errors);
            assert_eq!(out.plugins.fire("f12")[0].text, "ok");
            let _ = fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn ordinary_generic_for_and_metamethods_still_work() {
        let dir = scratch("meta-ok");
        let src = r#"
            local src = {__index = {n = 2}}
            local t = setmetatable({}, src)
            if rawequal(getmetatable(t), src) then error("caller metatable was installed") end
            getmetatable(t).__index = {n = 9}
            if t.n ~= 2 then error("view write changed the object") end
            local calls = 0
            local u = setmetatable({}, {__newindex = function() calls = calls + 1 end})
            u.x = true
            local f = setmetatable({}, {__call = function() return 3 end})
            local s = 0
            for _, v in ipairs({10, 20}) do s = s + v end
            for _, v in pairs({a = 1}) do s = s + v end
            for _, v in next, {7} do s = s + v end
            if calls ~= 1 or f() ~= 3 or ("xy"):sub(1, 1) ~= "x" then error("metamethod") end
            config.font_size = s
        "#;
        fs::write(dir.join("config.lua"), src).unwrap();
        let mut cfg = json_cfg(r#"{"font_size":12.0}"#);
        approve(&mut cfg, src.as_bytes());
        let out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.config.font_size, Some(38.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sealed_metatables_do_not_accumulate_across_keypresses() {
        let dir = scratch("meta-mem");
        let plugins = dir.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let src = r#"
            local Class = {}
            Class.__index = Class
            function Class:ping() return 1 end
            bayan.bind("f12", function()
              for _ = 1, 10 do
                local o = setmetatable({}, Class)
                o:ping()
              end
            end)
            bayan.bind("f11", function()
              collectgarbage("collect")
              collectgarbage("collect")
              bayan.send(tostring(collectgarbage("count")))
            end)
        "#;
        fs::write(plugins.join("hello.lua"), src).unwrap();
        let mut cfg = plugin_cfg(&plugins, "hello", false);
        approve(&mut cfg, src.as_bytes());
        let mut out = load_with(&cfg, &dir, false);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        for _ in 0..5000 {
            assert!(out.plugins.fire("f12").is_empty());
        }
        let sent = out.plugins.fire("f11");
        assert_eq!(sent.len(), 1, "the measurement callback must still run");
        let kb: f64 = sent[0].text.parse().expect(&sent[0].text);
        assert!(
            kb < 256.0,
            "sealed metatables leaked: collectgarbage count is {kb} KB"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rawset_of_close_is_refused_and_ordinary_rawset_works() {
        let got = probe(
            r#"
            local sm = getmetatable("")
            local ok = pcall(rawset, sm, "__close", function() end)
            if ok then error("rawset allowed __close on the string metatable") end
            local o = setmetatable({}, {__index = {}})
            local g = getmetatable(o)
            ok = pcall(rawset, g, "__close", function() end)
            if ok then error("rawset allowed __close on the view") end
            local t = {}
            rawset(t, "k", "v")
            if rawget(t, "k") ~= "v" then error("ordinary rawset failed") end
            return "ok"
            "#,
        )
        .unwrap();
        assert_eq!(got, "ok");
    }

    fn feed_terminal(bytes: &[u8]) {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;
        let size = crate::term::TermSize { cols: 40, rows: 6 };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser = Processor::new();
        let mut meta = crate::term::SessionMeta::default();
        let mut carry = Vec::new();
        let _ = crate::term::process_chunk(&mut parser, &mut term, &mut meta, &mut carry, bytes);
    }

    fn plugin_cfg(dir: &Path, name: &str, spawn: bool) -> UserConfig {
        UserConfig {
            lua_plugins_dir: Some(dir.display().to_string()),
            lua_plugins: vec![LuaPluginAllow {
                name: name.to_string(),
                spawn,
            }],
            ..UserConfig::default()
        }
    }
}
