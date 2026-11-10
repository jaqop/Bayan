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
//! `bayan.spawn(program, args)` runs `program` with `args` (a list of
//! strings) and no shell. It is absent unless this plugin's allowlist entry
//! sets `"spawn": true`.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    start: Instant,
    steps: AtomicU64,
    stride: u64,
    instructions: u64,
    deadline: Duration,
    exhausted: AtomicBool,
}

impl Fuel {
    fn over(&self) -> bool {
        if self.exhausted.load(Ordering::Acquire) {
            return true;
        }
        let steps = self.steps.load(Ordering::Relaxed);
        steps.saturating_mul(self.stride) > self.instructions
            || self.start.elapsed() > self.deadline
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
    /// Source the card shows, plus the hash. The approval is of the full
    /// file, not of this preview.
    pub preview: String,
}

impl PendingScript {
    pub fn reason(&self) -> String {
        let which = match &self.role {
            ScriptRole::Config => crate::lang::tr("config.lua", "config.lua"),
            ScriptRole::Plugin { name, .. } => name.as_str(),
        };
        format!(
            "{} — {which}",
            crate::lang::tr("تشغيل سكربت لوا", "Run a Lua script")
        )
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
        for plugin in &self.plugins {
            let hits: Vec<usize> = plugin
                .bindings
                .borrow()
                .iter()
                .enumerate()
                .filter(|(_, (c, _))| c == chord)
                .map(|(i, _)| i)
                .collect();
            for index in hits {
                call_binding(plugin, index);
                out.extend(drain_sends(plugin));
            }
        }
        out
    }

    /// Dispatch a host event. Callers must not use this for PTY bytes.
    pub fn emit(&mut self, event: &str, fields: &[(&str, &str)]) -> Vec<PluginSend> {
        let mut out = Vec::new();
        for plugin in &self.plugins {
            let hits: Vec<usize> = plugin
                .handlers
                .borrow()
                .iter()
                .enumerate()
                .filter(|(_, (name, _))| name == event)
                .map(|(i, _)| i)
                .collect();
            for index in hits {
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

    /// The PTY reader must not hand bytes to Lua. Tests pass the bytes the
    /// emulator just parsed; the body deliberately does not dispatch.
    #[cfg(test)]
    pub fn observe_terminal(&mut self, _bytes: &[u8]) {}
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
    pub config: UserConfig,
    pub errors: Vec<String>,
    /// A policy notice, not a script failure. Set when the token is elevated.
    pub notice: Option<String>,
    pub pending: Vec<PendingScript>,
    pub plugins: PluginSet,
}

/// User Lua and every plugin (executable or declarative) load only when
/// `elevated` is false. Tests pass the flag; the process asks [`is_elevated`].
pub fn scripts_enabled(elevated: bool) -> bool {
    !elevated
}

/// Whether this process token is elevated. That includes `--elevated` after
/// UAC and Explorer's Run as administrator. A failed query is not elevated:
/// the token bit is the only signal. Tests do not call this; they pass the
/// boolean into [`load_with`].
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        token_is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
fn token_is_elevated() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: GetCurrentProcess returns a pseudo-handle that is not closed.
    // OpenProcessToken writes one token handle, and CloseHandle releases it.
    // GetTokenInformation writes into `elevation`; the size is the struct's.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut written = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut written,
        );
        let _ = CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
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
        log_line(ELEVATED_NOTICE);
        return Outcome {
            config: cfg.clone(),
            errors: Vec::new(),
            notice: Some(ELEVATED_NOTICE.to_string()),
            pending: Vec::new(),
            plugins: PluginSet::default(),
        };
    }
    let mut errors = Vec::new();
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
            log_line(&format!("config.lua: {err}"));
            errors.push(format!("config.lua: {err}"));
        }
        ConfigLua::Applied(merged) => config = *merged,
    }

    // Allowlist, directory, and approvals come from the JSON config, not
    // from whatever config.lua just merged.
    let plugins = load_plugins(cfg, dir, &mut pending, &mut errors);
    Outcome {
        config,
        errors,
        notice: None,
        pending,
        plugins,
    }
}

/// The name a config watcher should call. Same rules as [`load_at`],
/// including the elevation gate: a changed file is not run, and an
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
    let mut preview: String = text.chars().take(12 * 56).collect();
    if text.chars().count() > preview.chars().count() {
        preview.push('\n');
        preview.push('…');
    }
    preview.push_str("\nsha256:");
    preview.push_str(&hash);
    PendingScript {
        role,
        path: path.to_path_buf(),
        hash,
        preview,
    }
}

fn apply_config_lua(cfg: &UserConfig, bytes: &[u8]) -> Result<UserConfig, String> {
    let source = source_text(bytes)?;
    let original = serde_json::to_value(cfg).map_err(|e| e.to_string())?;
    let lua = open_sandbox(Limits::production())?;
    let table = json_to_lua(&lua, &original)?;
    let Value::Table(config_table) = table else {
        return Err("config did not become a table".into());
    };
    lua.globals()
        .set("config", config_table)
        .map_err(|e| e.to_string())?;
    // No bayan table. spawn, bind, on, and send are not installed.
    let returned: Value = lua
        .load(source)
        .set_mode(ChunkMode::Text)
        .set_name("config.lua")
        .eval()
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
) -> PluginSet {
    if cfg.lua_plugins.is_empty() {
        return PluginSet::default();
    }
    let Some(root) = plugins_root(cfg, bayan_dir) else {
        let msg = "lua_plugins is set but lua_plugins_dir is not; no executable plugin ran";
        log_line(msg);
        errors.push(msg.into());
        return PluginSet::default();
    };
    let mut set = PluginSet::default();
    for allow in &cfg.lua_plugins {
        match load_one(cfg, &root, allow) {
            Ok(OnePlugin::Pending(script)) => pending.push(script),
            Ok(OnePlugin::Loaded(plugin)) => set.plugins.push(plugin),
            Err(err) => {
                log_line(&format!("{}: {err}", allow.name));
                errors.push(format!("{}: {err}", allow.name));
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
    let lua = open_sandbox(Limits::production())?;
    let regs = PluginRegs {
        bindings: Rc::new(RefCell::new(Vec::new())),
        handlers: Rc::new(RefCell::new(Vec::new())),
        sends: Arc::new(Mutex::new(Vec::new())),
        runs: Arc::new(AtomicU64::new(0)),
    };
    install_plugin_api(&lua, allow.spawn, &regs)?;
    lua.load(source)
        .set_mode(ChunkMode::Text)
        .set_name(&allow.name)
        .exec()
        .map_err(|e| e.to_string())?;
    Ok(LoadedPlugin {
        name: allow.name.clone(),
        bindings: regs.bindings,
        handlers: regs.handlers,
        sends: regs.sends,
        runs: regs.runs,
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
                let key = lua.create_registry_value(func)?;
                handlers.borrow_mut().push((event, key));
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

fn spawn_process(program: &str, args: &[String]) -> Result<String, String> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout {
            let _ = out.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let start = Instant::now();
    loop {
        match rx.try_recv() {
            Ok(buf) => {
                let _ = child.wait();
                let n = buf.len().min(64 * 1024);
                return String::from_utf8(buf[..n].to_vec())
                    .map_err(|_| "spawn output is not utf-8".to_string());
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("spawn produced no output".into());
            }
            Err(std::sync::mpsc::TryRecvError::Empty)
                if start.elapsed() > Duration::from_secs(2) =>
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err("spawn timed out".into());
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
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
fn open_sandbox(limits: Limits) -> Result<Lua, String> {
    let libs = StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8;
    // LuaOptions::default() in 0.12.2 (async feature off) is
    // catch_rust_panics: true. Panics in a callback become Lua errors
    // instead of unwinding through the C VM.
    let lua = Lua::new_with(libs, LuaOptions::default()).map_err(|e| e.to_string())?;
    lua.set_memory_limit(limits.memory)
        .map_err(|e| e.to_string())?;

    let fuel = Arc::new(Fuel {
        start: Instant::now(),
        steps: AtomicU64::new(0),
        stride: u64::from(limits.stride),
        instructions: limits.instructions,
        deadline: limits.deadline,
        exhausted: AtomicBool::new(false),
    });
    let hook_fuel = Arc::clone(&fuel);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(limits.stride),
        move |_lua, _dbg| {
            let steps = hook_fuel.steps.fetch_add(1, Ordering::Relaxed) + 1;
            let over = hook_fuel.exhausted.load(Ordering::Acquire)
                || steps.saturating_mul(hook_fuel.stride) > hook_fuel.instructions
                || hook_fuel.start.elapsed() > hook_fuel.deadline;
            if over {
                hook_fuel.exhausted.store(true, Ordering::Release);
                Err(mlua::Error::runtime("lua execution limit exceeded"))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
    .map_err(|e| e.to_string())?;

    harden(&lua, &fuel)?;
    Ok(lua)
}

fn harden(lua: &Lua, fuel: &Arc<Fuel>) -> Result<(), String> {
    let g = lua.globals();
    for name in [
        "io", "os", "debug", "package", "require", "loadfile", "dofile", "xpcall",
    ] {
        g.set(name, Value::Nil).map_err(|e| e.to_string())?;
    }
    let fuel = Arc::clone(fuel);
    g.set(
        "pcall",
        lua.create_function(move |lua, args: MultiValue| guarded_pcall(lua, &fuel, args))
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
    Ok(())
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
    loader.into_function()
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
        Value::Table(t) => table_to_json(&t),
        other => Err(format!("config.lua produced {other:?}")),
    }
}

fn table_to_json(t: &mlua::Table) -> Result<serde_json::Value, String> {
    if is_sequence(t)? {
        let n = t.raw_len();
        let mut items = Vec::with_capacity(n);
        for i in 1..=n {
            let v: Value = t.get(i).map_err(|e| e.to_string())?;
            items.push(lua_to_json(v)?);
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
        map.insert(key.to_string(), lua_to_json(v)?);
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
    let lua = open_sandbox(limits)?;
    let value: Value = lua
        .load(src)
        .set_mode(ChunkMode::Text)
        .set_name("probe")
        .eval()
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
    }

    #[test]
    fn pcall_cannot_swallow_the_execution_limit() {
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
    }

    #[test]
    fn the_memory_limit_rejects_a_huge_string() {
        let lua = open_sandbox(Limits {
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
        let lua = open_sandbox(Limits::production()).unwrap();
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
            out.plugins.observe_terminal(sample);
        }
        assert_eq!(out.plugins.callback_runs(), runs);
        assert_eq!(out.plugins.fire("ctrl+alt+g").len(), 1);
        // The fire above is the control: the binding still works, so the
        // silence was the sequences, not a plugin that failed to load.
        assert!(out
            .plugins
            .emit("bell", &[])
            .iter()
            .any(|s| s.text == "ran:bell"));
        // And the sequences themselves still did not emit. Re-feed after
        // the explicit emit and require the run count to stay put.
        let after = out.plugins.callback_runs();
        for sample in samples {
            feed_terminal(sample);
            out.plugins.observe_terminal(sample);
        }
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
