//! Per-user session multiplexer.
//!
//! `bayan --mux-server` owns ConPTY sessions. The window talks to it over
//! a named pipe whose DACL allows only the current user's SID and whose
//! mode sets `PIPE_REJECT_REMOTE_CLIENTS`. Detach leaves the session in
//! the server; reattach replays the output tail; kill ends it. The server
//! process exits when nothing is left alive. `mux.enabled` defaults to
//! false, and while it is false the window does not start or dial a server.
//!
//! What this is not: a ConPTY created inside the window cannot be handed
//! across with `DuplicateHandle` yet. Sessions that can be detached are
//! the ones the server spawned (SSH, when the mux is on). File-and-scrollback
//! restore is a different backlog item and is not done here.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

/// Output kept per session and replayed on reattach. A longer history is
/// the file-restore work, not this pipe.
pub const TAIL_MAX: usize = 256 * 1024;
const MAX_FRAME: usize = 1024 * 1024;
const MAX_SESSIONS: usize = 32;
const PROTO_VERSION: u8 = 1;

/// `dwPipeMode` bit: refuse clients that are not on this machine.
/// Win32 `PIPE_REJECT_REMOTE_CLIENTS`.
pub const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;

pub fn label_detach() -> &'static str {
    crate::lang::tr("فصل الجلسة", "Detach session")
}

pub fn label_reattach() -> &'static str {
    crate::lang::tr("إعادة الاتصال", "Reconnect")
}

pub fn label_kill() -> &'static str {
    crate::lang::tr("إنهاء الجلسة", "End session")
}

/// Palette rows for the mux. Detach and reattach are absent unless the
/// user opted in, so a default config cannot ask the window to start a server.
#[cfg(test)]
pub fn palette_labels(enabled: bool) -> Vec<&'static str> {
    let mut labels = vec![crate::ssh::label_open()];
    if enabled {
        labels.push(label_detach());
        labels.push(label_reattach());
    }
    labels
}

/// What to do with the server process. `None` does not connect and does not spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    None,
    Connect,
    Spawn,
}

pub fn server_launch(enabled: bool, pipe_up: bool) -> Launch {
    if !enabled {
        Launch::None
    } else if pipe_up {
        Launch::Connect
    } else {
        Launch::Spawn
    }
}

/// The decision `ensure_mux` makes. When `enabled` is false the `connect`
/// and `spawn` hooks are not called — a disabled mux must not probe the
/// pipe and must not start `bayan --mux-server`.
#[cfg(test)]
pub fn connect_if_enabled(
    enabled: bool,
    mut connect: impl FnMut() -> bool,
    mut spawn: impl FnMut(),
) -> bool {
    match server_launch(enabled, false) {
        Launch::None => false,
        Launch::Connect | Launch::Spawn => {
            if connect() {
                return true;
            }
            spawn();
            connect()
        }
    }
}

/// One detached or attached session, as the protocol carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnap {
    pub id: u64,
    pub title: String,
    pub command: String,
    pub cols: u16,
    pub rows: u16,
    pub screen: Vec<u8>,
}

/// What the window asks the server to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub title: String,
    pub cwd: String,
    pub program: String,
    pub args: Vec<String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Spawn { token: u64, spec: SpawnSpec },
    Spawned { token: u64, id: u64 },
    Fail { token: u64, message: String },
    Detach { id: u64 },
    Detached { id: u64 },
    List,
    Sessions { sessions: Vec<SessionSnap> },
    Reattach { id: u64 },
    Reattached { snap: SessionSnap },
    Missing { id: u64 },
    Kill { id: u64 },
    Killed { id: u64 },
    Input { id: u64, bytes: Vec<u8> },
    Output { id: u64, bytes: Vec<u8> },
    Exit { id: u64, code: u32 },
    Resize { id: u64, cols: u16, rows: u16 },
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Start { id: u64, spec: SpawnSpec },
    Stop { id: u64 },
    Write { id: u64, bytes: Vec<u8> },
    Resize { id: u64, cols: u16, rows: u16 },
}

#[derive(Debug)]
struct Slot {
    title: String,
    command: String,
    cols: u16,
    rows: u16,
    screen: Vec<u8>,
    attached: bool,
}

/// In-memory sessions. The pipe server and the in-process fake both drive this.
#[derive(Debug)]
pub struct MuxHub {
    next: u64,
    slots: BTreeMap<u64, Slot>,
}

impl MuxHub {
    pub fn new() -> Self {
        Self {
            next: 0,
            slots: BTreeMap::new(),
        }
    }

    pub fn alive(&self) -> usize {
        self.slots.len()
    }

    fn snap_of(id: u64, slot: &Slot) -> SessionSnap {
        SessionSnap {
            id,
            title: slot.title.clone(),
            command: slot.command.clone(),
            cols: slot.cols,
            rows: slot.rows,
            screen: slot.screen.clone(),
        }
    }

    fn spawn(&mut self, spec: &SpawnSpec) -> Result<u64, String> {
        if !valid_spawn(&spec.program, &spec.args) {
            return Err("refusing that program".into());
        }
        if self.slots.len() >= MAX_SESSIONS {
            return Err("too many mux sessions".into());
        }
        self.next += 1;
        let id = self.next;
        let command = command_display(&spec.program, &spec.args);
        self.slots.insert(
            id,
            Slot {
                title: spec.title.chars().take(120).collect(),
                command,
                cols: spec.cols.max(1),
                rows: spec.rows.max(1),
                screen: Vec::new(),
                attached: true,
            },
        );
        Ok(id)
    }

    fn note_output(&mut self, id: u64, bytes: &[u8]) -> bool {
        let Some(slot) = self.slots.get_mut(&id) else {
            return false;
        };
        push_tail(&mut slot.screen, bytes);
        slot.attached
    }

    fn detach(&mut self, id: u64) -> bool {
        let Some(slot) = self.slots.get_mut(&id) else {
            return false;
        };
        slot.attached = false;
        true
    }

    fn list_detached(&self) -> Vec<SessionSnap> {
        self.slots
            .iter()
            .filter(|(_, slot)| !slot.attached)
            .map(|(&id, slot)| Self::snap_of(id, slot))
            .collect()
    }

    fn reattach(&mut self, id: u64) -> Option<SessionSnap> {
        let slot = self.slots.get_mut(&id)?;
        if slot.attached {
            return None;
        }
        slot.attached = true;
        Some(Self::snap_of(id, slot))
    }

    fn kill(&mut self, id: u64) -> bool {
        self.slots.remove(&id).is_some()
    }

    fn child_gone(&mut self, id: u64) -> bool {
        self.slots.remove(&id).is_some()
    }

    /// Sessions the departing window still had on screen. Detached ones stay.
    fn drop_attached(&mut self) -> Vec<u64> {
        let ids: Vec<u64> = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.attached)
            .map(|(&id, _)| id)
            .collect();
        for id in &ids {
            self.slots.remove(id);
        }
        ids
    }

    fn resize(&mut self, id: u64, cols: u16, rows: u16) -> bool {
        let Some(slot) = self.slots.get_mut(&id) else {
            return false;
        };
        slot.cols = cols.max(1);
        slot.rows = rows.max(1);
        true
    }
}

impl Default for MuxHub {
    fn default() -> Self {
        Self::new()
    }
}

struct Gate {
    ever_had: bool,
}

pub struct Outcome {
    pub replies: Vec<Message>,
    pub effects: Vec<Effect>,
    /// True when the last session is gone and the process should exit.
    /// A server that has never owned a session stays up, waiting for one.
    pub exit_now: bool,
}

fn wants_exit(gate: &Gate, hub: &MuxHub) -> bool {
    gate.ever_had && hub.alive() == 0
}

fn reduce(hub: &mut MuxHub, gate: &mut Gate, msg: Message) -> Outcome {
    let mut replies = Vec::new();
    let mut effects = Vec::new();
    match msg {
        Message::Spawn { token, spec } => match hub.spawn(&spec) {
            Ok(id) => {
                gate.ever_had = true;
                effects.push(Effect::Start { id, spec });
                replies.push(Message::Spawned { token, id });
            }
            Err(message) => replies.push(Message::Fail { token, message }),
        },
        Message::Detach { id } => {
            if hub.detach(id) {
                replies.push(Message::Detached { id });
            } else {
                replies.push(Message::Missing { id });
            }
        }
        Message::List => replies.push(Message::Sessions {
            sessions: hub.list_detached(),
        }),
        Message::Reattach { id } => match hub.reattach(id) {
            Some(snap) => replies.push(Message::Reattached { snap }),
            None => replies.push(Message::Missing { id }),
        },
        Message::Kill { id } => {
            if hub.kill(id) {
                effects.push(Effect::Stop { id });
                replies.push(Message::Killed { id });
            } else {
                replies.push(Message::Missing { id });
            }
        }
        Message::Input { id, bytes } => {
            if hub.slots.contains_key(&id) {
                effects.push(Effect::Write { id, bytes });
            } else {
                replies.push(Message::Missing { id });
            }
        }
        Message::Resize { id, cols, rows } => {
            if hub.resize(id, cols, rows) {
                effects.push(Effect::Resize { id, cols, rows });
            } else {
                replies.push(Message::Missing { id });
            }
        }
        other => replies.push(Message::Error {
            message: format!("server ignores {other:?}"),
        }),
    }
    let exit_now = wants_exit(gate, hub);
    Outcome {
        replies,
        effects,
        exit_now,
    }
}

fn client_gone(hub: &mut MuxHub) -> Outcome {
    let effects = hub
        .drop_attached()
        .into_iter()
        .map(|id| Effect::Stop { id })
        .collect();
    // Detached sessions keep the process up. None at all and it leaves,
    // including a client that connected and started nothing.
    let exit_now = hub.alive() == 0;
    Outcome {
        replies: Vec::new(),
        effects,
        exit_now,
    }
}

fn valid_spawn(program: &str, args: &[String]) -> bool {
    !program.is_empty()
        && program.len() <= 1024
        && !program.starts_with('-')
        && !program.chars().any(|c| c.is_control())
        && args.len() <= 64
        && args
            .iter()
            .all(|arg| !arg.is_empty() && arg.len() <= 4096 && !arg.chars().any(|c| c.is_control()))
}

fn command_display(program: &str, args: &[String]) -> String {
    let mut line = program.to_string();
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

pub fn push_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    if tail.len() <= TAIL_MAX {
        return;
    }
    let mut drop_n = tail.len() - TAIL_MAX;
    if let Some(rel) = tail[drop_n..].iter().position(|&b| b == b'\n') {
        if rel < 4096 {
            drop_n += rel + 1;
        }
    }
    tail.drain(..drop_n.min(tail.len()));
}

/// SDDL for a pipe DACL: protected, one allow-all ACE, that SID only.
/// `WD` (everyone), `BA` (admins) and a second ACE are not produced.
/// The SID must already be an `S-1-…` string; anything else is rejected so
/// a caller cannot smuggle extra ACEs through the SID text.
pub fn sddl_for_sid(sid: &str) -> Result<String, String> {
    if !sid.starts_with("S-1-")
        || sid.len() < 6
        || sid.len() > 184
        || !sid
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'S' | b'-'))
    {
        return Err("security identifier is not a single S-1 sid".into());
    }
    Ok(format!("D:P(A;;GA;;;{sid})"))
}

/// `dwOpenMode` and `dwPipeMode` for the server end. The mode word carries
/// `PIPE_REJECT_REMOTE_CLIENTS`; the open word asks to be the first instance.
pub fn server_pipe_flags() -> (u32, u32) {
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    const PIPE_TYPE_BYTE: u32 = 0;
    const PIPE_READMODE_BYTE: u32 = 0;
    const PIPE_WAIT: u32 = 0;
    let open = PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE;
    let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    (open, mode)
}

pub fn encode_frame(msg: &Message, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    encode_message(msg, out);
    let body = out.len() - start - 4;
    out[start..start + 4].copy_from_slice(&(body as u32).to_le_bytes());
}

#[cfg(test)]
pub fn decode_frame(buf: &[u8]) -> Result<Option<(Message, usize)>, &'static str> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let body_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    if body_len == 0 || body_len > MAX_FRAME {
        return Err("frame length");
    }
    if buf.len() < 4 + body_len {
        return Ok(None);
    }
    let msg = decode_message(&buf[4..4 + body_len])?;
    Ok(Some((msg, 4 + body_len)))
}

fn encode_message(msg: &Message, out: &mut Vec<u8>) {
    out.push(PROTO_VERSION);
    match msg {
        Message::Spawn { token, spec } => {
            out.push(1);
            put_u64(out, *token);
            put_spec(out, spec);
        }
        Message::Spawned { token, id } => {
            out.push(2);
            put_u64(out, *token);
            put_u64(out, *id);
        }
        Message::Fail { token, message } => {
            out.push(3);
            put_u64(out, *token);
            put_str(out, message);
        }
        Message::Detach { id } => {
            out.push(4);
            put_u64(out, *id);
        }
        Message::Detached { id } => {
            out.push(5);
            put_u64(out, *id);
        }
        Message::List => out.push(6),
        Message::Sessions { sessions } => {
            out.push(7);
            put_u32(out, sessions.len() as u32);
            for snap in sessions {
                put_snap(out, snap);
            }
        }
        Message::Reattach { id } => {
            out.push(8);
            put_u64(out, *id);
        }
        Message::Reattached { snap } => {
            out.push(9);
            put_snap(out, snap);
        }
        Message::Missing { id } => {
            out.push(10);
            put_u64(out, *id);
        }
        Message::Kill { id } => {
            out.push(11);
            put_u64(out, *id);
        }
        Message::Killed { id } => {
            out.push(12);
            put_u64(out, *id);
        }
        Message::Input { id, bytes } => {
            out.push(13);
            put_u64(out, *id);
            put_bytes(out, bytes);
        }
        Message::Output { id, bytes } => {
            out.push(14);
            put_u64(out, *id);
            put_bytes(out, bytes);
        }
        Message::Exit { id, code } => {
            out.push(15);
            put_u64(out, *id);
            put_u32(out, *code);
        }
        Message::Resize { id, cols, rows } => {
            out.push(16);
            put_u64(out, *id);
            put_u16(out, *cols);
            put_u16(out, *rows);
        }
        Message::Error { message } => {
            out.push(17);
            put_str(out, message);
        }
    }
}

fn decode_message(buf: &[u8]) -> Result<Message, &'static str> {
    let mut r = Reader { buf, i: 0 };
    let version = r.u8()?;
    if version != PROTO_VERSION {
        return Err("version");
    }
    let msg = match r.u8()? {
        1 => Message::Spawn {
            token: r.u64()?,
            spec: r.spec()?,
        },
        2 => Message::Spawned {
            token: r.u64()?,
            id: r.u64()?,
        },
        3 => Message::Fail {
            token: r.u64()?,
            message: r.str()?,
        },
        4 => Message::Detach { id: r.u64()? },
        5 => Message::Detached { id: r.u64()? },
        6 => Message::List,
        7 => {
            let n = r.u32()? as usize;
            if n > MAX_SESSIONS {
                return Err("session count");
            }
            let mut sessions = Vec::with_capacity(n);
            for _ in 0..n {
                sessions.push(r.snap()?);
            }
            Message::Sessions { sessions }
        }
        8 => Message::Reattach { id: r.u64()? },
        9 => Message::Reattached { snap: r.snap()? },
        10 => Message::Missing { id: r.u64()? },
        11 => Message::Kill { id: r.u64()? },
        12 => Message::Killed { id: r.u64()? },
        13 => Message::Input {
            id: r.u64()?,
            bytes: r.bytes()?,
        },
        14 => Message::Output {
            id: r.u64()?,
            bytes: r.bytes()?,
        },
        15 => Message::Exit {
            id: r.u64()?,
            code: r.u32()?,
        },
        16 => Message::Resize {
            id: r.u64()?,
            cols: r.u16()?,
            rows: r.u16()?,
        },
        17 => Message::Error { message: r.str()? },
        _ => return Err("opcode"),
    };
    if r.i != r.buf.len() {
        return Err("trailing");
    }
    Ok(msg)
}

struct Reader<'a> {
    buf: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, &'static str> {
        let b = *self.buf.get(self.i).ok_or("truncated")?;
        self.i += 1;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16, &'static str> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes(b.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, &'static str> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, &'static str> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let end = self.i.checked_add(n).ok_or("truncated")?;
        if end > self.buf.len() {
            return Err("truncated");
        }
        let slice = &self.buf[self.i..end];
        self.i = end;
        Ok(slice)
    }
    fn bytes(&mut self) -> Result<Vec<u8>, &'static str> {
        let n = self.u32()? as usize;
        if n > MAX_FRAME {
            return Err("blob");
        }
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Result<String, &'static str> {
        let bytes = self.bytes()?;
        String::from_utf8(bytes).map_err(|_| "utf8")
    }
    fn spec(&mut self) -> Result<SpawnSpec, &'static str> {
        let cols = self.u16()?;
        let rows = self.u16()?;
        let title = self.str()?;
        let cwd = self.str()?;
        let program = self.str()?;
        let n = self.u32()? as usize;
        if n > 64 {
            return Err("argc");
        }
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            args.push(self.str()?);
        }
        Ok(SpawnSpec {
            title,
            cwd,
            program,
            args,
            cols,
            rows,
        })
    }
    fn snap(&mut self) -> Result<SessionSnap, &'static str> {
        Ok(SessionSnap {
            id: self.u64()?,
            title: self.str()?,
            command: self.str()?,
            cols: self.u16()?,
            rows: self.u16()?,
            screen: self.bytes()?,
        })
    }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}
fn put_spec(out: &mut Vec<u8>, spec: &SpawnSpec) {
    put_u16(out, spec.cols);
    put_u16(out, spec.rows);
    put_str(out, &spec.title);
    put_str(out, &spec.cwd);
    put_str(out, &spec.program);
    put_u32(out, spec.args.len() as u32);
    for arg in &spec.args {
        put_str(out, arg);
    }
}
fn put_snap(out: &mut Vec<u8>, snap: &SessionSnap) {
    put_u64(out, snap.id);
    put_str(out, &snap.title);
    put_str(out, &snap.command);
    put_u16(out, snap.cols);
    put_u16(out, snap.rows);
    put_bytes(out, &snap.screen);
}

pub fn write_frame(mut w: impl Write, msg: &Message) -> std::io::Result<()> {
    let mut buf = Vec::new();
    encode_frame(msg, &mut buf);
    w.write_all(&buf)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> std::io::Result<Message> {
    let mut len_b = [0u8; 4];
    r.read_exact(&mut len_b)?;
    let len = u32::from_le_bytes(len_b) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame length",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    decode_message(&body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// The state machine the window talks to, in-process, speaking the real frames.
/// PTY bytes are injected with [`FakeMuxServer::inject_output`] instead of a ConPTY.
#[cfg(test)]
pub struct FakeMuxServer {
    hub: MuxHub,
    gate: Gate,
    pub effects: Vec<Effect>,
    pub exit_now: bool,
}

#[cfg(test)]
impl FakeMuxServer {
    pub fn new() -> Self {
        Self {
            hub: MuxHub::new(),
            gate: Gate { ever_had: false },
            effects: Vec::new(),
            exit_now: false,
        }
    }

    pub fn transact(&mut self, msg: Message) -> Vec<Message> {
        let mut wire = Vec::new();
        encode_frame(&msg, &mut wire);
        let (decoded, n) = decode_frame(&wire).expect("frame").expect("complete");
        assert_eq!(n, wire.len());
        assert_eq!(decoded, msg);
        let outcome = reduce(&mut self.hub, &mut self.gate, decoded);
        self.effects.extend(outcome.effects);
        self.exit_now = outcome.exit_now;
        let mut back = Vec::new();
        for reply in &outcome.replies {
            encode_frame(reply, &mut back);
        }
        let mut got = Vec::new();
        let mut rest = back.as_slice();
        while !rest.is_empty() {
            let (m, used) = decode_frame(rest).expect("reply frame").expect("complete");
            got.push(m);
            rest = &rest[used..];
        }
        assert_eq!(got, outcome.replies);
        got
    }

    pub fn inject_output(&mut self, id: u64, bytes: &[u8]) {
        self.hub.note_output(id, bytes);
    }

    pub fn client_disconnected(&mut self) {
        let outcome = client_gone(&mut self.hub);
        self.effects.extend(outcome.effects);
        self.exit_now = outcome.exit_now;
    }
}

#[cfg(test)]
impl Default for FakeMuxServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Ends of a server session, as the window's pane wants them.
pub struct Attached {
    pub id: u64,
    pub title: String,
    pub command: String,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub on_resize: Box<dyn Fn(u16, u16) + Send + Sync>,
    pub on_kill: Box<dyn FnOnce() + Send>,
    /// Cleared to detach: the kill hook then leaves the server process alone.
    pub armed: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct MuxClient {
    tx: mpsc::Sender<Cmd>,
    alive: Arc<AtomicBool>,
}

enum Cmd {
    Spawn {
        spec: SpawnSpec,
        done: mpsc::Sender<Result<Attached, String>>,
    },
    Reattach {
        id: u64,
        done: mpsc::Sender<Result<Attached, String>>,
    },
    Detach {
        id: u64,
        done: mpsc::Sender<Result<(), String>>,
    },
    Kill {
        id: u64,
    },
    List {
        done: mpsc::Sender<Result<Vec<SessionSnap>, String>>,
    },
    Input {
        id: u64,
        bytes: Vec<u8>,
    },
    Resize {
        id: u64,
        cols: u16,
        rows: u16,
    },
}

struct ChanRead {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChanRead {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.buf = next;
                    self.pos = 0;
                    if self.buf.is_empty() {
                        continue;
                    }
                }
                Err(_) => return Ok(0),
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct MuxWrite {
    id: u64,
    tx: mpsc::Sender<Cmd>,
}

impl Write for MuxWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tx
            .send(Cmd::Input {
                id: self.id,
                bytes: buf.to_vec(),
            })
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "mux server is gone")
            })?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl MuxClient {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn spawn(&self, spec: SpawnSpec) -> Result<Attached, String> {
        let (done, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Spawn { spec, done })
            .map_err(|_| "mux client stopped".to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "mux server did not answer spawn".to_string())?
    }

    pub fn reattach(&self, id: u64) -> Result<Attached, String> {
        let (done, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Reattach { id, done })
            .map_err(|_| "mux client stopped".to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "mux server did not answer reattach".to_string())?
    }

    pub fn detach(&self, id: u64) -> Result<(), String> {
        let (done, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Detach { id, done })
            .map_err(|_| "mux client stopped".to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "mux server did not answer detach".to_string())?
    }

    pub fn kill_remote(&self, id: u64) {
        let _ = self.tx.send(Cmd::Kill { id });
    }

    pub fn list(&self) -> Result<Vec<SessionSnap>, String> {
        let (done, rx) = mpsc::channel();
        self.tx
            .send(Cmd::List { done })
            .map_err(|_| "mux client stopped".to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "mux server did not answer list".to_string())?
    }
}

fn finish_attached(
    id: u64,
    title: String,
    command: String,
    output: mpsc::Receiver<Vec<u8>>,
    tx: mpsc::Sender<Cmd>,
) -> Attached {
    let armed = Arc::new(AtomicBool::new(true));
    let armed_kill = Arc::clone(&armed);
    let kill_tx = tx.clone();
    let resize_tx = tx.clone();
    Attached {
        id,
        title,
        command,
        reader: Box::new(ChanRead {
            rx: output,
            buf: Vec::new(),
            pos: 0,
        }),
        writer: Box::new(MuxWrite { id, tx }),
        on_resize: Box::new(move |cols, rows| {
            let _ = resize_tx.send(Cmd::Resize { id, cols, rows });
        }),
        on_kill: Box::new(move || {
            if armed_kill.swap(false, Ordering::Relaxed) {
                let _ = kill_tx.send(Cmd::Kill { id });
            }
        }),
        armed,
    }
}

/// Entry point for `bayan --mux-server`.
pub fn run_mux_server() -> Result<(), String> {
    #[cfg(windows)]
    {
        os::run()
    }
    #[cfg(not(windows))]
    {
        Err("the mux server listens on a Windows named pipe".into())
    }
}

/// Dial the current user's pipe, starting the server process if it is down.
/// `enabled` false does not dial and does not spawn (`Launch::None`).
pub fn connect_or_spawn(enabled: bool) -> Result<MuxClient, String> {
    if server_launch(enabled, false) == Launch::None {
        return Err("mux is disabled".into());
    }
    #[cfg(windows)]
    {
        os::connect_or_spawn()
    }
    #[cfg(not(windows))]
    {
        Err("the mux client dials a Windows named pipe".into())
    }
}

#[cfg(windows)]
mod os {
    use super::*;
    use std::collections::HashMap;
    use std::fs::File;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::sync::Mutex;

    const REPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn run() -> Result<(), String> {
        let (tx, rx) = mpsc::channel::<Ev>();
        start_acceptor(tx.clone());
        let mut hub = MuxHub::new();
        let mut gate = Gate { ever_had: false };
        let mut lives: HashMap<u64, Live> = HashMap::new();
        let mut writer: Option<File> = None;
        let mut accept_fails = 0u32;
        while let Ok(ev) = rx.recv() {
            match ev {
                Ev::Connected(file) => {
                    accept_fails = 0;
                    match file.try_clone() {
                        Ok(read) => {
                            writer = Some(file);
                            let tx2 = tx.clone();
                            let _ = std::thread::Builder::new()
                                .name("bayan-mux-read".into())
                                .spawn(move || reader_loop(read, tx2));
                        }
                        Err(e) => {
                            let _ = tx.send(Ev::AcceptErr(e.to_string()));
                        }
                    }
                }
                Ev::Frame(msg) => {
                    let mut outcome = reduce(&mut hub, &mut gate, msg);
                    if let Err(message) = apply(&mut lives, &tx, &mut outcome.effects) {
                        if let Some(Message::Spawned { token, id }) = outcome
                            .replies
                            .iter()
                            .find(|m| matches!(m, Message::Spawned { .. }))
                            .cloned()
                        {
                            hub.kill(id);
                            if hub.alive() == 0 {
                                gate.ever_had = false;
                            }
                            outcome.replies = vec![Message::Fail { token, message }];
                            outcome.exit_now = false;
                        }
                    }
                    if let Some(w) = writer.as_mut() {
                        for reply in &outcome.replies {
                            if write_frame(&mut *w, reply).is_err() {
                                writer = None;
                                break;
                            }
                        }
                    }
                    if outcome.exit_now || (gate.ever_had && hub.alive() == 0) {
                        break;
                    }
                }
                Ev::Output(id, bytes) => {
                    let attached = hub.note_output(id, &bytes);
                    if attached {
                        if let Some(w) = writer.as_mut() {
                            let _ = write_frame(&mut *w, &Message::Output { id, bytes });
                        }
                    }
                }
                Ev::ChildGone(id) => {
                    if hub.child_gone(id) {
                        lives.remove(&id);
                        if let Some(w) = writer.as_mut() {
                            let _ = write_frame(&mut *w, &Message::Exit { id, code: 0 });
                        }
                    }
                    if gate.ever_had && hub.alive() == 0 {
                        break;
                    }
                }
                Ev::Eof => {
                    writer = None;
                    let outcome = client_gone(&mut hub);
                    let mut effects = outcome.effects;
                    let _ = apply(&mut lives, &tx, &mut effects);
                    if outcome.exit_now {
                        break;
                    }
                    start_acceptor(tx.clone());
                }
                Ev::AcceptErr(error) => {
                    accept_fails += 1;
                    // Nobody left to keep, or the pipe never came up: leave.
                    // Detached sessions retry; a name clash can clear.
                    if hub.alive() == 0 && (gate.ever_had || accept_fails > 8) {
                        return Err(error);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    start_acceptor(tx.clone());
                }
            }
        }
        Ok(())
    }

    enum Ev {
        Connected(File),
        Frame(Message),
        Eof,
        Output(u64, Vec<u8>),
        ChildGone(u64),
        AcceptErr(String),
    }

    struct Live {
        writer: Box<dyn Write + Send>,
        master: Box<dyn portable_pty::MasterPty + Send>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
    }

    fn reader_loop(mut read: File, tx: mpsc::Sender<Ev>) {
        loop {
            match read_frame(&mut read) {
                Ok(msg) => {
                    if tx.send(Ev::Frame(msg)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send(Ev::Eof);
                    break;
                }
            }
        }
    }

    fn apply(
        lives: &mut HashMap<u64, Live>,
        tx: &mpsc::Sender<Ev>,
        effects: &mut Vec<Effect>,
    ) -> Result<(), String> {
        for effect in effects.drain(..) {
            match effect {
                Effect::Start { id, spec } => {
                    let (live, mut reader) = start_pty(&spec)?;
                    let tx2 = tx.clone();
                    let _ = std::thread::Builder::new()
                        .name("bayan-mux-pty".into())
                        .spawn(move || {
                            let mut buf = [0u8; 8192];
                            loop {
                                match reader.read(&mut buf) {
                                    Ok(0) | Err(_) => {
                                        let _ = tx2.send(Ev::ChildGone(id));
                                        break;
                                    }
                                    Ok(n) => {
                                        if tx2.send(Ev::Output(id, buf[..n].to_vec())).is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        });
                    lives.insert(id, live);
                }
                Effect::Stop { id } => {
                    if let Some(mut live) = lives.remove(&id) {
                        let _ = live.child.kill();
                    }
                }
                Effect::Write { id, bytes } => {
                    if let Some(live) = lives.get_mut(&id) {
                        let _ = live.writer.write_all(&bytes);
                        let _ = live.writer.flush();
                    }
                }
                Effect::Resize { id, cols, rows } => {
                    if let Some(live) = lives.get(&id) {
                        let _ = live.master.resize(portable_pty::PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn start_pty(spec: &SpawnSpec) -> Result<(Live, Box<dyn Read + Send>), String> {
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(portable_pty::PtySize {
                rows: spec.rows.max(1),
                cols: spec.cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        let mut cmd = portable_pty::CommandBuilder::new(&spec.program);
        for arg in &spec.args {
            cmd.arg(arg);
        }
        if let Some(dir) = crate::term::accept_local_cwd(&spec.cwd) {
            if crate::term::usable_spawn_dir(&dir) {
                cmd.cwd(dir);
            }
        }
        cmd.env("TERM_PROGRAM", "Bayan");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        cmd.env("COLORTERM", "truecolor");
        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        Ok((
            Live {
                writer,
                master: pair.master,
                child,
            },
            reader,
        ))
    }

    fn start_acceptor(tx: mpsc::Sender<Ev>) {
        let _ = std::thread::Builder::new()
            .name("bayan-mux-accept".into())
            .spawn(move || match create_connected_pipe() {
                Ok(file) => {
                    let _ = tx.send(Ev::Connected(file));
                }
                Err(e) => {
                    let _ = tx.send(Ev::AcceptErr(e));
                }
            });
    }

    pub fn connect_or_spawn() -> Result<MuxClient, String> {
        if let Ok(client) = MuxClient::dial() {
            return Ok(client);
        }
        spawn_server_process()?;
        let mut last = "mux server did not accept a connection".to_string();
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            match MuxClient::dial() {
                Ok(client) => return Ok(client),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn spawn_server_process() -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--mux-server")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.spawn().map_err(|e| e.to_string())?;
        Ok(())
    }

    type Routes = Arc<Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>;

    impl MuxClient {
        fn dial() -> Result<Self, String> {
            let write = connect_pipe()?;
            let mut read = write.try_clone().map_err(|e| e.to_string())?;
            let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
            let (control_tx, control_rx) = mpsc::channel::<Message>();
            let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
            let pending: Routes = Arc::new(Mutex::new(HashMap::new()));
            let alive = Arc::new(AtomicBool::new(true));
            let alive_read = Arc::clone(&alive);
            let routes_read = Arc::clone(&routes);
            let pending_read = Arc::clone(&pending);
            std::thread::Builder::new()
                .name("bayan-mux-client-read".into())
                .spawn(move || loop {
                    match read_frame(&mut read) {
                        Ok(msg) => {
                            dispatch_incoming(msg, &routes_read, &pending_read, &control_tx);
                        }
                        Err(_) => {
                            alive_read.store(false, Ordering::Relaxed);
                            let _ = control_tx.send(Message::Error {
                                message: "mux pipe closed".into(),
                            });
                            break;
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            let cmd_for_thread = cmd_tx.clone();
            std::thread::Builder::new()
                .name("bayan-mux-client".into())
                .spawn(move || {
                    command_loop(write, cmd_rx, control_rx, routes, pending, cmd_for_thread)
                })
                .map_err(|e| e.to_string())?;
            Ok(MuxClient { tx: cmd_tx, alive })
        }
    }

    fn lock_map(map: &Routes) -> std::sync::MutexGuard<'_, HashMap<u64, mpsc::Sender<Vec<u8>>>> {
        map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dispatch_incoming(
        msg: Message,
        routes: &Routes,
        pending: &Routes,
        control: &mpsc::Sender<Message>,
    ) {
        match msg {
            Message::Output { id, bytes } => {
                if let Some(tx) = lock_map(routes).get(&id) {
                    let _ = tx.send(bytes);
                }
            }
            Message::Spawned { token, id } => {
                if let Some(tx) = lock_map(pending).remove(&token) {
                    lock_map(routes).insert(id, tx);
                }
                let _ = control.send(Message::Spawned { token, id });
            }
            Message::Reattached { snap } => {
                let id = snap.id;
                if let Some(tx) = lock_map(pending).remove(&id) {
                    let _ = tx.send(snap.screen.clone());
                    lock_map(routes).insert(id, tx);
                }
                let _ = control.send(Message::Reattached { snap });
            }
            Message::Exit { id, code } => {
                lock_map(routes).remove(&id);
                let _ = control.send(Message::Exit { id, code });
            }
            other => {
                let _ = control.send(other);
            }
        }
    }

    fn command_loop(
        mut write: File,
        cmd_rx: mpsc::Receiver<Cmd>,
        control_rx: mpsc::Receiver<Message>,
        routes: Routes,
        pending: Routes,
        cmd_tx: mpsc::Sender<Cmd>,
    ) {
        let mut token = 1u64;
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                Cmd::Spawn { spec, done } => {
                    token = token.wrapping_add(1);
                    let (out_tx, out_rx) = mpsc::channel();
                    lock_map(&pending).insert(token, out_tx);
                    let title = spec.title.clone();
                    let command = command_display(&spec.program, &spec.args);
                    let result = (|| {
                        write_frame(&mut write, &Message::Spawn { token, spec })
                            .map_err(|e| e.to_string())?;
                        match wait_reply(&control_rx, |m| {
                            matches!(
                                m,
                                Message::Spawned { token: t, .. } | Message::Fail { token: t, .. }
                                    if *t == token
                            )
                        })? {
                            Message::Spawned { id, .. } => {
                                Ok(finish_attached(id, title, command, out_rx, cmd_tx.clone()))
                            }
                            Message::Fail { message, .. } => Err(message),
                            _ => Err("unexpected spawn reply".into()),
                        }
                    })();
                    if result.is_err() {
                        lock_map(&pending).remove(&token);
                    }
                    let _ = done.send(result);
                }
                Cmd::Reattach { id, done } => {
                    let (out_tx, out_rx) = mpsc::channel();
                    lock_map(&pending).insert(id, out_tx);
                    let result = (|| {
                        write_frame(&mut write, &Message::Reattach { id })
                            .map_err(|e| e.to_string())?;
                        match wait_reply(&control_rx, |m| {
                            matches!(m, Message::Reattached { snap } if snap.id == id)
                                || matches!(m, Message::Missing { id: m } if *m == id)
                        })? {
                            Message::Reattached { snap } => Ok(finish_attached(
                                snap.id,
                                snap.title,
                                snap.command,
                                out_rx,
                                cmd_tx.clone(),
                            )),
                            Message::Missing { .. } => Err("session is not detached".into()),
                            _ => Err("unexpected reattach reply".into()),
                        }
                    })();
                    if result.is_err() {
                        lock_map(&pending).remove(&id);
                    }
                    let _ = done.send(result);
                }
                Cmd::Detach { id, done } => {
                    lock_map(&routes).remove(&id);
                    let result = (|| {
                        write_frame(&mut write, &Message::Detach { id })
                            .map_err(|e| e.to_string())?;
                        match wait_reply(&control_rx, |m| {
                            matches!(m, Message::Detached { id: d } if *d == id)
                                || matches!(m, Message::Missing { id: m } if *m == id)
                        })? {
                            Message::Detached { .. } => Ok(()),
                            Message::Missing { .. } => Err("no such session".into()),
                            _ => Err("unexpected detach reply".into()),
                        }
                    })();
                    let _ = done.send(result);
                }
                Cmd::Kill { id } => {
                    lock_map(&routes).remove(&id);
                    if write_frame(&mut write, &Message::Kill { id }).is_ok() {
                        let _ = wait_reply(&control_rx, |m| {
                            matches!(m, Message::Killed { id: k } if *k == id)
                                || matches!(m, Message::Missing { id: m } if *m == id)
                        });
                    }
                }
                Cmd::List { done } => {
                    let result = (|| {
                        write_frame(&mut write, &Message::List).map_err(|e| e.to_string())?;
                        match wait_reply(&control_rx, |m| matches!(m, Message::Sessions { .. }))? {
                            Message::Sessions { sessions } => Ok(sessions),
                            _ => Err("unexpected list reply".into()),
                        }
                    })();
                    let _ = done.send(result);
                }
                Cmd::Input { id, bytes } => {
                    let _ = write_frame(&mut write, &Message::Input { id, bytes });
                }
                Cmd::Resize { id, cols, rows } => {
                    let _ = write_frame(&mut write, &Message::Resize { id, cols, rows });
                }
            }
        }
    }

    fn wait_reply(
        rx: &mpsc::Receiver<Message>,
        pred: impl Fn(&Message) -> bool,
    ) -> Result<Message, String> {
        let deadline = std::time::Instant::now() + REPLY_WAIT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err("mux server timed out".into());
            }
            match rx.recv_timeout(left) {
                Ok(msg) if pred(&msg) => return Ok(msg),
                Ok(Message::Error { message }) => return Err(message),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => return Err("mux server timed out".into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("mux pipe closed".into());
                }
            }
        }
    }

    fn connect_pipe() -> Result<File, String> {
        let name = production_pipe_name()?;
        let wide = wstr(&name);
        // SAFETY: `wide` is NUL-terminated and lives for the call. WaitNamedPipeW
        // only reads the name.
        unsafe {
            windows_sys::Win32::System::Pipes::WaitNamedPipeW(wide.as_ptr(), 200);
        }
        let handle = create_file_pipe(&wide)?;
        // SAFETY: `handle` is an open pipe we own. File closes it.
        Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
    }

    fn create_connected_pipe() -> Result<File, String> {
        let name = production_pipe_name()?;
        let handle = create_named_pipe(&name)?;
        // SAFETY: `handle` came from CreateNamedPipeW and is not yet connected.
        // A null overlapped pointer is the synchronous form.
        let ok = unsafe {
            windows_sys::Win32::System::Pipes::ConnectNamedPipe(handle, std::ptr::null_mut())
        };
        if ok == 0 {
            let err = last_error();
            // 535 ERROR_PIPE_CONNECTED: the client won the race, which is success.
            if err != 535 {
                // SAFETY: the handle is not wrapped yet, so this is the only close.
                unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
                return Err(format!("ConnectNamedPipe failed ({err})"));
            }
        }
        // SAFETY: `handle` is an open connected pipe this function owns.
        // `File` closes it.
        Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
    }

    #[cfg(test)]
    pub(super) struct SecuredPipe {
        pub file: File,
        pub mode: u32,
        pub sid: Vec<u8>,
    }

    #[cfg(test)]
    pub(super) fn listen_secured(name: &str) -> Result<SecuredPipe, String> {
        let sid = current_user_sid()?;
        let (_open, mode) = server_pipe_flags();
        let handle = create_named_pipe(name)?;
        Ok(SecuredPipe {
            // SAFETY: we own `handle` and have not connected it. Drop closes it.
            file: unsafe { File::from_raw_handle(handle as RawHandle) },
            mode,
            sid,
        })
    }

    pub(super) fn current_user_sid() -> Result<Vec<u8>, String> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: GetCurrentProcess is this process. TOKEN_QUERY needs no
        // extra rights. The SID pointer addresses `buf`, which stays alive
        // until the bytes are copied and the token handle is closed.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(format!("OpenProcessToken failed ({})", last_error()));
            }
            let mut needed = 0u32;
            // The first call sizes the buffer. Failure with no length is real.
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                CloseHandle(token);
                return Err("GetTokenInformation returned no length".into());
            }
            let mut buf = vec![0u8; needed as usize];
            if GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                CloseHandle(token);
                return Err(format!("GetTokenInformation failed ({})", last_error()));
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let len = GetLengthSid(user.User.Sid) as usize;
            if len == 0 || len > 256 {
                CloseHandle(token);
                return Err("token user sid has no length".into());
            }
            let bytes = std::slice::from_raw_parts(user.User.Sid as *const u8, len).to_vec();
            CloseHandle(token);
            Ok(bytes)
        }
    }

    /// True when the pipe's DACL is exactly one allow ACE and that ACE is `sid`.
    #[cfg(test)]
    pub(super) fn dacl_is_only(file: &File, sid: &[u8]) -> Result<bool, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        use windows_sys::Win32::Security::{GetAce, ACL};
        const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
        // SAFETY: `file` is a live pipe handle. GetSecurityInfo allocates
        // `sd`; the DACL pointer addresses memory inside that block, and
        // LocalFree releases the block after the ACE bytes are compared.
        unsafe {
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd = std::ptr::null_mut();
            let err = GetSecurityInfo(
                file.as_raw_handle() as _,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            );
            if err != 0 || dacl.is_null() {
                if !sd.is_null() {
                    LocalFree(sd as _);
                }
                return Err(format!("GetSecurityInfo failed ({err})"));
            }
            let count = (*dacl).AceCount as u32;
            let mut only = count == 1;
            if only {
                let mut ace = std::ptr::null_mut();
                if GetAce(dacl, 0, &mut ace) == 0 || ace.is_null() {
                    only = false;
                } else {
                    let raw = ace as *const u8;
                    let ace_type = *raw;
                    let ace_size = u16::from_le_bytes([*raw.add(2), *raw.add(3)]) as usize;
                    if ace_type != ACCESS_ALLOWED_ACE_TYPE || ace_size < 8 + sid.len() {
                        only = false;
                    } else {
                        let got = std::slice::from_raw_parts(raw.add(8), sid.len());
                        only = got == sid;
                    }
                }
            }
            LocalFree(sd as _);
            Ok(only)
        }
    }

    fn production_pipe_name() -> Result<String, String> {
        let sid = current_user_sid()?;
        let mut wide_ptr = std::ptr::null_mut();
        // SAFETY: `sid` is a token SID we just copied. The API allocates the
        // string; LocalFree releases it.
        let ok = unsafe {
            windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(
                sid.as_ptr() as _,
                &mut wide_ptr,
            )
        };
        if ok == 0 || wide_ptr.is_null() {
            return Err(format!("ConvertSidToStringSidW failed ({})", last_error()));
        }
        let text = wide_to_string(wide_ptr);
        // SAFETY: ConvertSidToStringSidW allocated `wide_ptr`. The text is
        // already copied into `text`, so this frees only that allocation.
        unsafe { windows_sys::Win32::Foundation::LocalFree(wide_ptr as _) };
        if sddl_for_sid(&text).is_err() {
            return Err("token sid did not render as S-1-…".into());
        }
        Ok(format!(r"\\.\pipe\bayan-mux-{text}"))
    }

    fn create_named_pipe(name: &str) -> Result<windows_sys::Win32::Foundation::HANDLE, String> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
        let sid = current_user_sid()?;
        let mut wide_ptr = std::ptr::null_mut();
        // SAFETY: `sid` is a token SID we copied. The API allocates `wide_ptr`.
        let ok = unsafe {
            windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(
                sid.as_ptr() as _,
                &mut wide_ptr,
            )
        };
        if ok == 0 || wide_ptr.is_null() {
            return Err(format!("ConvertSidToStringSidW failed ({})", last_error()));
        }
        let sid_text = wide_to_string(wide_ptr);
        // SAFETY: the SID string is copied into `sid_text`. This frees the
        // allocation ConvertSidToStringSidW made.
        unsafe { LocalFree(wide_ptr as _) };
        let sddl = sddl_for_sid(&sid_text)?;
        let sddl_w = wstr(&sddl);
        let mut sd = std::ptr::null_mut();
        let mut sd_size = 0u32;
        // SAFETY: `sddl_w` is NUL-terminated. The API allocates `sd`.
        let made = unsafe {
            windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_w.as_ptr(),
                1,
                &mut sd,
                &mut sd_size,
            )
        };
        if made == 0 || sd.is_null() {
            return Err(format!(
                "ConvertStringSecurityDescriptor failed ({})",
                last_error()
            ));
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let (open_mode, pipe_mode) = server_pipe_flags();
        let name_w = wstr(name);
        // SAFETY: both wide strings and `sa` (pointing at `sd`) live for the
        // call. The descriptor is copied onto the pipe object.
        let handle = unsafe {
            CreateNamedPipeW(
                name_w.as_ptr(),
                open_mode,
                pipe_mode,
                1,
                64 * 1024,
                64 * 1024,
                0,
                &sa,
            )
        };
        // SAFETY: ConvertStringSecurityDescriptor allocated `sd`, and
        // CreateNamedPipeW has already copied it onto the pipe.
        unsafe { LocalFree(sd as _) };
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(format!("CreateNamedPipeW failed ({})", last_error()));
        }
        Ok(handle)
    }

    fn create_file_pipe(name: &[u16]) -> Result<windows_sys::Win32::Foundation::HANDLE, String> {
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
        };
        const GENERIC_READ: u32 = 0x8000_0000;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        // SAFETY: `name` is NUL-terminated. A null security descriptor means
        // the client's own token, which the server's DACL already allows.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(format!(
                "CreateFileW on the mux pipe failed ({})",
                last_error()
            ));
        }
        Ok(handle)
    }

    fn wstr(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn wide_to_string(wide: *const u16) -> String {
        let mut n = 0usize;
        // SAFETY: `wide` is a NUL-terminated string just returned by
        // ConvertSidToStringSidW, and the walk stops at 200 units.
        unsafe {
            while *wide.add(n) != 0 {
                n += 1;
                if n > 200 {
                    break;
                }
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(wide, n))
        }
    }

    fn last_error() -> u32 {
        // SAFETY: GetLastError reads thread-local state and takes no pointers.
        unsafe { windows_sys::Win32::Foundation::GetLastError() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec(program: &str) -> SpawnSpec {
        SpawnSpec {
            title: "example.com".into(),
            cwd: String::new(),
            program: program.into(),
            args: vec!["--".into(), "example.com".into()],
            cols: 80,
            rows: 24,
        }
    }

    #[test]
    fn protocol_messages_round_trip() {
        let messages = vec![
            Message::List,
            Message::Spawn {
                token: 7,
                spec: sample_spec(r"C:\Windows\System32\OpenSSH\ssh.exe"),
            },
            Message::Spawned { token: 7, id: 3 },
            Message::Fail {
                token: 7,
                message: "no".into(),
            },
            Message::Detach { id: 3 },
            Message::Detached { id: 3 },
            Message::Sessions {
                sessions: vec![SessionSnap {
                    id: 3,
                    title: "example.com".into(),
                    command: "ssh.exe -- example.com".into(),
                    cols: 80,
                    rows: 24,
                    screen: b"hello\r\n\x00\xff".to_vec(),
                }],
            },
            Message::Reattach { id: 3 },
            Message::Reattached {
                snap: SessionSnap {
                    id: 3,
                    title: "example.com".into(),
                    command: "ssh".into(),
                    cols: 40,
                    rows: 10,
                    screen: vec![0, 1, 2, 255],
                },
            },
            Message::Missing { id: 9 },
            Message::Kill { id: 3 },
            Message::Killed { id: 3 },
            Message::Input {
                id: 3,
                bytes: b"ls\r".to_vec(),
            },
            Message::Output {
                id: 3,
                bytes: b"ok".to_vec(),
            },
            Message::Exit { id: 3, code: 1 },
            Message::Resize {
                id: 3,
                cols: 100,
                rows: 30,
            },
            Message::Error {
                message: "x".into(),
            },
        ];
        for msg in messages {
            let mut buf = Vec::new();
            encode_frame(&msg, &mut buf);
            let (back, n) = decode_frame(&buf).unwrap().unwrap();
            assert_eq!(n, buf.len());
            assert_eq!(back, msg);
            let mut cursor = std::io::Cursor::new(buf);
            assert_eq!(read_frame(&mut cursor).unwrap(), msg);
        }
        assert!(decode_frame(&[1, 0, 0, 0]).is_err() || decode_frame(&[0, 0, 0, 0]).is_err());
        assert!(decode_frame(&[4, 0, 0, 0, 9, 1, 2, 3]).is_err());
    }

    #[test]
    fn detach_list_reattach_on_the_in_process_server() {
        let mut srv = FakeMuxServer::new();
        let replies = srv.transact(Message::Spawn {
            token: 1,
            spec: sample_spec(r"C:\Windows\System32\OpenSSH\ssh.exe"),
        });
        let id = match &replies[0] {
            Message::Spawned { id, token } => {
                assert_eq!(*token, 1);
                *id
            }
            other => panic!("spawn reply {other:?}"),
        };
        assert!(srv
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Start { id: i, .. } if *i == id)));
        srv.inject_output(id, b"banner\r\n");
        let replies = srv.transact(Message::Detach { id });
        assert_eq!(replies, vec![Message::Detached { id }]);
        assert!(!srv.exit_now, "a detached session keeps the server");
        let replies = srv.transact(Message::List);
        let Message::Sessions { sessions } = &replies[0] else {
            panic!("list {replies:?}");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].title, "example.com");
        assert_eq!(sessions[0].screen, b"banner\r\n");
        let replies = srv.transact(Message::Reattach { id });
        let Message::Reattached { snap } = &replies[0] else {
            panic!("reattach {replies:?}");
        };
        assert_eq!(snap.screen, b"banner\r\n");
        assert_eq!(snap.title, "example.com");
        let replies = srv.transact(Message::List);
        let Message::Sessions { sessions } = &replies[0] else {
            panic!("list after reattach {replies:?}");
        };
        assert!(
            sessions.is_empty(),
            "an attached session is not in the detached list"
        );
    }

    #[test]
    fn server_exits_when_the_last_session_ends() {
        let mut srv = FakeMuxServer::new();
        assert!(!srv.exit_now);
        let replies = srv.transact(Message::Spawn {
            token: 1,
            spec: sample_spec("ssh.exe"),
        });
        let Message::Spawned { id, .. } = replies[0] else {
            panic!("spawn");
        };
        assert!(!srv.exit_now);
        srv.transact(Message::Detach { id });
        assert!(!srv.exit_now, "detach is not the end of the session");
        let replies = srv.transact(Message::Kill { id });
        assert_eq!(replies, vec![Message::Killed { id }]);
        assert!(srv.exit_now, "no sessions left");
        assert!(srv
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Stop { id: i } if *i == id)));
    }

    #[test]
    fn closing_the_client_keeps_detached_sessions_and_stops_without_them() {
        let mut srv = FakeMuxServer::new();
        let spawn = |srv: &mut FakeMuxServer, token| match &srv.transact(Message::Spawn {
            token,
            spec: sample_spec("ssh.exe"),
        })[0]
        {
            Message::Spawned { id, .. } => *id,
            other => panic!("{other:?}"),
        };
        let detached = spawn(&mut srv, 1);
        let attached = spawn(&mut srv, 2);
        srv.transact(Message::Detach { id: detached });
        srv.client_disconnected();
        assert!(!srv.exit_now, "the detached session is still alive");
        assert!(srv
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Stop { id } if *id == attached)));
        assert!(!srv
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Stop { id } if *id == detached)));
        srv.transact(Message::Kill { id: detached });
        assert!(srv.exit_now);
    }

    #[test]
    fn mux_disabled_does_not_spawn_a_server() {
        assert_eq!(server_launch(false, false), Launch::None);
        assert_eq!(server_launch(false, true), Launch::None);
        assert_eq!(server_launch(true, true), Launch::Connect);
        assert_eq!(server_launch(true, false), Launch::Spawn);
        let mut spawned = false;
        let mut probed = false;
        let up = connect_if_enabled(
            false,
            || {
                probed = true;
                true
            },
            || spawned = true,
        );
        assert!(!up);
        assert!(!spawned);
        assert!(!probed, "a disabled mux does not touch the pipe");
        let mut spawned = false;
        let up = connect_if_enabled(true, || false, || spawned = true);
        assert!(!up, "spawn still needs the second connect to succeed");
        assert!(spawned);
        let labels = palette_labels(false);
        assert_eq!(labels, vec![crate::ssh::label_open()]);
        assert!(palette_labels(true).contains(&label_detach()));
        assert!(palette_labels(true).contains(&label_reattach()));
        crate::lang::set(crate::lang::Lang::Ar);
        assert_eq!(label_detach(), "فصل الجلسة");
        assert_eq!(label_reattach(), "إعادة الاتصال");
        assert_eq!(label_kill(), "إنهاء الجلسة");
    }

    #[test]
    fn sddl_allows_only_the_given_sid_and_the_pipe_rejects_remote_clients() {
        let sddl = sddl_for_sid("S-1-5-21-1-2-3-1001").unwrap();
        assert_eq!(sddl, "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)");
        assert_eq!(sddl.matches("(A;").count(), 1);
        assert!(!sddl.contains("WD"), "everyone");
        assert!(!sddl.contains("BA"), "builtin admins");
        assert!(sddl_for_sid("Everyone").is_err());
        assert!(sddl_for_sid("S-1-5-21-1;WD").is_err());
        assert!(sddl_for_sid("S-1-5-21-1)(A;;GA;;;WD").is_err());
        let (_open, mode) = server_pipe_flags();
        assert_eq!(
            mode & PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_REJECT_REMOTE_CLIENTS
        );
        let (open, _) = server_pipe_flags();
        assert_ne!(open & 0x0008_0000, 0, "first pipe instance");
    }

    #[test]
    fn option_like_programs_are_not_spawned() {
        let mut srv = FakeMuxServer::new();
        let replies = srv.transact(Message::Spawn {
            token: 1,
            spec: SpawnSpec {
                title: "x".into(),
                cwd: String::new(),
                program: "-oProxyCommand=calc".into(),
                args: vec![],
                cols: 80,
                rows: 24,
            },
        });
        assert!(matches!(replies[0], Message::Fail { .. }));
        assert!(!srv.exit_now);
        assert!(srv.effects.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn pipe_security_descriptor_allows_only_the_current_user_and_rejects_remote_clients() {
        let sid = os::current_user_sid().expect("token user sid");
        let name = format!(
            r"\\.\pipe\bayan-mux-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let pipe = os::listen_secured(&name).expect("create the pipe");
        assert_eq!(
            pipe.mode & PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_REJECT_REMOTE_CLIENTS,
            "CreateNamedPipeW is given PIPE_REJECT_REMOTE_CLIENTS"
        );
        assert_eq!(pipe.sid, sid);
        assert!(
            os::dacl_is_only(&pipe.file, &sid).expect("read the DACL"),
            "DACL must be one allow ACE for the token user and nobody else"
        );
    }
}
