//! LAN networking for Rally (see docs/plans/lan-rally.md).
//!
//! - **Discovery:** the host broadcasts `HEARTLINE1 <name> <port>` on UDP 47821 once a second;
//!   `Discovery` listens and keeps a list of hosts with their age.
//! - **Connection:** one TCP connection (port 47820, `TCP_NODELAY`) carries everything: lobby,
//!   song file, clock sync, inputs and snapshots. TCP gives ordering and reliability for free;
//!   on a LAN the round trip is about a millisecond.
//! - **Framing:** `[u32 len][u8 type][payload]`, little-endian, `len` counts the type byte and
//!   the payload. Every length is capped and every field bounds-checked; a malformed frame
//!   closes the connection with a message instead of panicking.
//! - **Threads:** each connection has a reader thread (blocking reads, so it wakes the instant
//!   data arrives) and a writer thread (blocks on its command channel). The game thread only
//!   uses `Net::send*` (channel send) and `Net::poll` (`try_recv`); it never touches a socket.
//!   Frame buffers are recycled through pool channels so the match loop does not allocate.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub const PROTO_VER: u8 = 1;
pub const TCP_PORT: u16 = 47820;
pub const DISC_PORT: u16 = 47821;
pub const BEACON_MAGIC: &str = "HEARTLINE1";
/// Largest frame (header included) either side accepts.
pub const MAX_FRAME: usize = 64 * 1024;
/// Largest song file the host may send.
pub const MAX_SONG: u64 = 200 * 1024 * 1024;
/// Song bytes per `Chunk` frame.
pub const CHUNK: usize = 48 * 1024;
pub const MAX_NAME: usize = 24;
pub const MAX_TITLE: usize = 120;
/// Largest Rally snapshot body accepted.
pub const MAX_SNAP: usize = 4096;
/// Seconds without any data before the connection counts as lost.
pub const TIMEOUT: f64 = 3.0;
/// Clock-sync window: the offset comes from the lowest-RTT sample of the last N pongs.
pub const SYNC_SAMPLES: usize = 8;
/// Keep-alive / re-sync ping interval once the initial burst is done.
const KEEPALIVE: f64 = 0.25;
/// `Song::theme` value meaning "a real file follows" (otherwise a built-in theme index).
pub const NO_THEME: u8 = 255;

/// Monotonic seconds since the first call in this process. Both clock-sync timestamps and the
/// agreed match start use this clock.
pub fn now() -> f64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64()
}

pub mod ty {
    pub const HELLO: u8 = 1;
    pub const LOBBY: u8 = 2;
    pub const SONG: u8 = 3;
    pub const CHUNK: u8 = 4;
    pub const LOADED: u8 = 5;
    pub const PING: u8 = 6;
    pub const PONG: u8 = 7;
    pub const START: u8 = 8;
    pub const INPUT: u8 = 9;
    pub const SNAP: u8 = 10;
    pub const END: u8 = 11;
    pub const BYE: u8 = 12;
}

/// One player's input as sent over the wire. `swings` counts swing presses (wrapping), so a
/// press is never lost or doubled however inputs and frames line up.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NetInput {
    pub seq: u32,
    pub mv: [f32; 2],
    pub aim: [f32; 2],
    pub swings: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Hello { ver: u8, name: String },
    Lobby { ch: u8, ready: bool },
    /// Song header; `size` bytes follow in `Chunk` frames (none for a built-in theme).
    Song { title: String, theme: u8, ext: String, size: u64 },
    Chunk(Vec<u8>),
    Loaded,
    Ping { t0: f64 },
    Pong { t0: f64, th: f64 },
    Start { host_time: f64, seed: u64, chars: [u8; 2] },
    Input(NetInput),
    /// Host snapshot: host song time, last input seq applied, `Rally::encode` body.
    Snap { t: f32, ack: u32, data: Vec<u8> },
    End { score: [u32; 2] },
    Bye,
}

// ---------------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------------

fn put_str(out: &mut Vec<u8>, s: &str, max: usize) {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    out.push(n as u8);
    out.extend_from_slice(&s.as_bytes()[..n]);
}

/// Append one framed message to `out`.
pub fn encode(m: &Msg, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    match m {
        Msg::Hello { ver, name } => {
            out.push(ty::HELLO);
            out.push(*ver);
            put_str(out, name, MAX_NAME);
        }
        Msg::Lobby { ch, ready } => {
            out.push(ty::LOBBY);
            out.push(*ch);
            out.push(*ready as u8);
        }
        Msg::Song { title, theme, ext, size } => {
            out.push(ty::SONG);
            put_str(out, title, MAX_TITLE);
            out.push(*theme);
            put_str(out, ext, 8);
            out.extend_from_slice(&size.to_le_bytes());
        }
        Msg::Chunk(d) => {
            out.push(ty::CHUNK);
            out.extend_from_slice(&d[..d.len().min(CHUNK)]);
        }
        Msg::Loaded => out.push(ty::LOADED),
        Msg::Ping { t0 } => {
            out.push(ty::PING);
            out.extend_from_slice(&t0.to_le_bytes());
        }
        Msg::Pong { t0, th } => {
            out.push(ty::PONG);
            out.extend_from_slice(&t0.to_le_bytes());
            out.extend_from_slice(&th.to_le_bytes());
        }
        Msg::Start { host_time, seed, chars } => {
            out.push(ty::START);
            out.extend_from_slice(&host_time.to_le_bytes());
            out.extend_from_slice(&seed.to_le_bytes());
            out.extend_from_slice(chars);
        }
        Msg::Input(i) => {
            out.push(ty::INPUT);
            out.extend_from_slice(&i.seq.to_le_bytes());
            let q = |v: f32| ((v.clamp(-1.0, 1.0) * 127.0).round() as i8) as u8;
            out.push(q(i.mv[0]));
            out.push(q(i.mv[1]));
            out.extend_from_slice(&i.aim[0].to_le_bytes());
            out.extend_from_slice(&i.aim[1].to_le_bytes());
            out.push(i.swings);
        }
        Msg::Snap { t, ack, data } => {
            out.push(ty::SNAP);
            out.extend_from_slice(&t.to_le_bytes());
            out.extend_from_slice(&ack.to_le_bytes());
            out.extend_from_slice(&data[..data.len().min(MAX_SNAP)]);
        }
        Msg::End { score } => {
            out.push(ty::END);
            out.extend_from_slice(&score[0].to_le_bytes());
            out.extend_from_slice(&score[1].to_le_bytes());
        }
        Msg::Bye => out.push(ty::BYE),
    }
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Append a `Snap` frame without building a `Msg` (no allocation).
pub fn encode_snap(t: f32, ack: u32, body: &[u8], out: &mut Vec<u8>) {
    let body = &body[..body.len().min(MAX_SNAP)];
    let len = (1 + 8 + body.len()) as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.push(ty::SNAP);
    out.extend_from_slice(&t.to_le_bytes());
    out.extend_from_slice(&ack.to_le_bytes());
    out.extend_from_slice(body);
}

// ---------------------------------------------------------------------------------------------
// Decoding (bounds-checked; `None` = malformed)
// ---------------------------------------------------------------------------------------------

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.i.checked_add(n)?;
        let s = self.b.get(self.i..end)?;
        self.i = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Some(u64::from_le_bytes(a))
    }
    fn f32(&mut self) -> Option<f32> {
        let v = f32::from_bits(self.u32()?);
        v.is_finite().then_some(v)
    }
    fn f64(&mut self) -> Option<f64> {
        let v = f64::from_bits(self.u64()?);
        v.is_finite().then_some(v)
    }
    fn str(&mut self, max: usize) -> Option<String> {
        let n = self.u8()? as usize;
        if n > max {
            return None;
        }
        let s = std::str::from_utf8(self.take(n)?).ok()?;
        // Printable text only: names and titles are drawn on screen, never used as paths.
        Some(s.chars().filter(|c| !c.is_control()).collect())
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.i.min(self.b.len())..];
        self.i = self.b.len();
        s
    }
    fn done(&self) -> bool {
        self.i == self.b.len()
    }
}

/// Decode one frame body (`payload` excludes the length and type bytes). `Chunk` and `Snap`
/// data are copied into `scratch` (taken from a buffer pool), so steady state never allocates.
pub fn decode(t: u8, payload: &[u8], scratch: &mut Option<Vec<u8>>) -> Option<Msg> {
    let mut r = Rd { b: payload, i: 0 };
    let m = match t {
        ty::HELLO => Msg::Hello { ver: r.u8()?, name: r.str(MAX_NAME)? },
        ty::LOBBY => {
            let ch = r.u8()?;
            let ready = r.u8()?;
            if ready > 1 {
                return None;
            }
            Msg::Lobby { ch, ready: ready == 1 }
        }
        ty::SONG => {
            let title = r.str(MAX_TITLE)?;
            let theme = r.u8()?;
            let ext = r.str(8)?;
            let size = r.u64()?;
            if size > MAX_SONG || (theme != NO_THEME && size != 0) {
                return None;
            }
            Msg::Song { title, theme, ext, size }
        }
        ty::CHUNK => {
            let d = r.rest();
            if d.is_empty() || d.len() > CHUNK {
                return None;
            }
            let mut v = scratch.take().unwrap_or_default();
            v.clear();
            v.extend_from_slice(d);
            Msg::Chunk(v)
        }
        ty::LOADED => Msg::Loaded,
        ty::PING => Msg::Ping { t0: r.f64()? },
        ty::PONG => Msg::Pong { t0: r.f64()?, th: r.f64()? },
        ty::START => {
            let host_time = r.f64()?;
            let seed = r.u64()?;
            let c = r.take(2)?;
            Msg::Start { host_time, seed, chars: [c[0], c[1]] }
        }
        ty::INPUT => {
            let seq = r.u32()?;
            let mx = r.u8()? as i8;
            let my = r.u8()? as i8;
            let ax = r.f32()?;
            let ay = r.f32()?;
            let swings = r.u8()?;
            let dq = |v: i8| (v as f32 / 127.0).clamp(-1.0, 1.0);
            Msg::Input(NetInput { seq, mv: [dq(mx), dq(my)], aim: [ax, ay], swings })
        }
        ty::SNAP => {
            let t = r.f32()?;
            let ack = r.u32()?;
            let d = r.rest();
            if d.len() > MAX_SNAP {
                return None;
            }
            let mut v = scratch.take().unwrap_or_default();
            v.clear();
            v.extend_from_slice(d);
            Msg::Snap { t, ack, data: v }
        }
        ty::END => Msg::End { score: [r.u32()?, r.u32()?] },
        ty::BYE => Msg::Bye,
        _ => return None,
    };
    r.done().then_some(m)
}

/// Pulls complete frames out of a byte stream. Returns `Err` on a frame that breaks the caps.
pub struct Framer {
    buf: Vec<u8>,
    have: usize,
    off: usize,
}

impl Framer {
    pub fn new() -> Framer {
        Framer { buf: vec![0; MAX_FRAME * 2], have: 0, off: 0 }
    }
    /// Space to read into.
    pub fn space(&mut self) -> &mut [u8] {
        if self.off > 0 {
            self.buf.copy_within(self.off..self.have, 0);
            self.have -= self.off;
            self.off = 0;
        }
        &mut self.buf[self.have..]
    }
    pub fn filled(&mut self, n: usize) {
        self.have = (self.have + n).min(self.buf.len());
    }
    /// Next complete frame as (type, payload).
    pub fn next(&mut self) -> Result<Option<(u8, &[u8])>, String> {
        let avail = self.have - self.off;
        if avail < 4 {
            return Ok(None);
        }
        let b = &self.buf[self.off..self.have];
        let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        if len == 0 || len + 4 > MAX_FRAME {
            return Err(format!("bad frame length {len}"));
        }
        if avail < 4 + len {
            return Ok(None);
        }
        let s = self.off;
        self.off += 4 + len;
        Ok(Some((self.buf[s + 4], &self.buf[s + 5..s + 4 + len])))
    }
}

impl Default for Framer {
    fn default() -> Self {
        Framer::new()
    }
}

// ---------------------------------------------------------------------------------------------
// Clock sync (NTP-style: offset from the lowest-RTT sample of the recent window)
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct ClockSync {
    samples: VecDeque<(f64, f64)>,
    pub total: u32,
}

impl ClockSync {
    /// `t0` local send time, `th` remote receive time, `t2` local receive time.
    pub fn add(&mut self, t0: f64, th: f64, t2: f64) {
        let rtt = (t2 - t0).max(0.0);
        let off = th - (t0 + t2) * 0.5;
        if self.samples.len() >= SYNC_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((rtt, off));
        self.total += 1;
    }
    /// (offset = remote clock - local clock, rtt) of the best sample.
    pub fn best(&self) -> Option<(f64, f64)> {
        self.samples.iter().copied().min_by(|a, b| a.0.total_cmp(&b.0)).map(|(r, o)| (o, r))
    }
}

// ---------------------------------------------------------------------------------------------
// Connection threads
// ---------------------------------------------------------------------------------------------

/// What the net threads tell the game thread.
#[derive(Debug)]
pub enum Event {
    /// Socket is up and listening (host) / connecting (client).
    Status(String),
    /// Hello exchanged; the peer's name.
    Connected(String),
    Msg(Msg),
    /// Song transfer progress (bytes done, total) on either side.
    SongProgress(u64, u64),
    /// Client: the song file is saved (`path` None for a built-in theme).
    SongSaved { path: Option<PathBuf>, title: String, theme: u8 },
    /// Host: the whole song has been written to the socket.
    SongSent,
    /// Clock offset (peer clock - local clock), best RTT, samples so far.
    Clock { offset: f64, rtt: f64, samples: u32 },
    Disconnected(String),
}

enum Cmd {
    Frame(Vec<u8>),
    SendSong { path: Option<PathBuf>, title: String, theme: u8, ext: String },
    Pong { t0: f64, th: f64 },
    Close,
}

struct Shared {
    dead: AtomicBool,
    sent: AtomicU64,
    recv: AtomicU64,
}

impl Shared {
    /// Report a disconnect once (whichever thread notices first).
    fn die(&self, ev: &Sender<Event>, why: String) {
        if !self.dead.swap(true, Ordering::SeqCst) {
            let _ = ev.send(Event::Disconnected(why));
        }
    }
}

/// Game-thread handle to one LAN session (host or client).
pub struct Net {
    cmd: Sender<Cmd>,
    ev: Receiver<Event>,
    /// Empty frame buffers coming back from the writer.
    free_out: Receiver<Vec<u8>>,
    /// Consumed `Chunk` / `Snap` buffers going back to the reader.
    free_in: Sender<Vec<u8>>,
    shared: Arc<Shared>,
    pub port: u16,
}

impl Net {
    /// Start hosting: binds TCP `port` now (so errors are immediate), then a thread accepts one
    /// player and broadcasts the discovery beacon until someone joins.
    pub fn host(name: &str, port: u16, beacon: bool) -> Result<Net, String> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|e| format!("cannot open port {port}: {e}"))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let (net, parts) = Net::make(port);
        let name = clean_name(name);
        std::thread::Builder::new()
            .name("net-host".into())
            .spawn(move || {
                let udp = if beacon {
                    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok().filter(|u| u.set_broadcast(true).is_ok())
                } else {
                    None
                };
                let msg = format!("{BEACON_MAGIC} {name} {port}");
                let _ = parts.ev_tx.send(Event::Status(format!("waiting for a player on port {port}")));
                let mut next_beacon = 0.0;
                loop {
                    match parts.cmd_rx.try_recv() {
                        Ok(Cmd::Close) | Err(TryRecvError::Disconnected) => return,
                        _ => {}
                    }
                    if let Some(u) = &udp {
                        if now() >= next_beacon {
                            let _ = u.send_to(msg.as_bytes(), (Ipv4Addr::BROADCAST, DISC_PORT));
                            next_beacon = now() + 1.0;
                        }
                    }
                    match listener.accept() {
                        Ok((s, _)) => {
                            drop(listener);
                            run_conn(s, name, parts);
                            return;
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                        Err(e) => {
                            parts.shared.die(&parts.ev_tx, format!("hosting failed: {e}"));
                            return;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(net)
    }

    /// Connect to a host (in the background; `Connected` or `Disconnected` follows).
    pub fn join(addr: SocketAddr, name: &str) -> Net {
        let (net, parts) = Net::make(addr.port());
        let name = clean_name(name);
        let _ = std::thread::Builder::new().name("net-join".into()).spawn(move || {
            let _ = parts.ev_tx.send(Event::Status(format!("connecting to {addr}")));
            match TcpStream::connect_timeout(&addr, Duration::from_secs_f64(TIMEOUT)) {
                Ok(s) => run_conn(s, name, parts),
                Err(e) => parts.shared.die(&parts.ev_tx, format!("could not reach {addr}: {e}")),
            }
        });
        net
    }

    fn make(port: u16) -> (Net, Parts) {
        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let (out_tx, out_rx) = channel();
        let (in_tx, in_rx) = channel();
        let shared = Arc::new(Shared { dead: AtomicBool::new(false), sent: AtomicU64::new(0), recv: AtomicU64::new(0) });
        let net = Net { cmd: cmd_tx.clone(), ev: ev_rx, free_out: out_rx, free_in: in_tx, shared: shared.clone(), port };
        (net, Parts { cmd_rx, cmd_tx, ev_tx, out_tx, in_rx, shared })
    }

    /// Next event, if any (never blocks).
    pub fn poll(&self) -> Option<Event> {
        self.ev.try_recv().ok()
    }

    fn frame_buf(&self) -> Vec<u8> {
        let mut b = self.free_out.try_recv().unwrap_or_else(|_| Vec::with_capacity(1024));
        b.clear();
        b
    }

    pub fn send(&self, m: &Msg) {
        let mut b = self.frame_buf();
        encode(m, &mut b);
        let _ = self.cmd.send(Cmd::Frame(b));
    }

    pub fn send_snap(&self, t: f32, ack: u32, body: &[u8]) {
        let mut b = self.frame_buf();
        encode_snap(t, ack, body, &mut b);
        let _ = self.cmd.send(Cmd::Frame(b));
    }

    /// Stream a song file (or announce a built-in theme with `path` None). A new call replaces
    /// a transfer in progress.
    pub fn send_song(&self, path: Option<PathBuf>, title: &str, theme: u8, ext: &str) {
        let _ = self.cmd.send(Cmd::SendSong { path, title: title.to_string(), theme, ext: ext.to_string() });
    }

    /// Give a `Chunk`/`Snap` buffer back to the reader for reuse.
    pub fn recycle(&self, b: Vec<u8>) {
        let _ = self.free_in.send(b);
    }

    /// Bytes (sent, received) on the TCP connection so far.
    pub fn traffic(&self) -> (u64, u64) {
        (self.shared.sent.load(Ordering::Relaxed), self.shared.recv.load(Ordering::Relaxed))
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        // Local close: no Disconnected event; the writer says Bye and shuts the socket.
        self.shared.dead.store(true, Ordering::SeqCst);
        let _ = self.cmd.send(Cmd::Close);
    }
}

struct Parts {
    cmd_rx: Receiver<Cmd>,
    cmd_tx: Sender<Cmd>,
    ev_tx: Sender<Event>,
    out_tx: Sender<Vec<u8>>,
    in_rx: Receiver<Vec<u8>>,
    shared: Arc<Shared>,
}

fn clean_name(s: &str) -> String {
    let n: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(MAX_NAME).collect();
    if n.is_empty() {
        "player".into()
    } else {
        n
    }
}

/// This PC's display name for the lobby and the beacon.
pub fn local_name() -> String {
    clean_name(&std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default())
}

/// A byte stream the connection threads can run on: TCP in the game, an in-memory pipe in tests.
/// Reads must time out after `TIMEOUT` with `TimedOut`/`WouldBlock`; `close` must wake a reader.
trait Duplex: Read + Write + Send + Sized + 'static {
    fn dup(&self) -> std::io::Result<Self>;
    fn close(&self);
}

impl Duplex for TcpStream {
    fn dup(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
    fn close(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

fn run_conn(s: TcpStream, name: String, p: Parts) {
    let setup = (|| -> std::io::Result<()> {
        s.set_nonblocking(false)?;
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(Duration::from_secs_f64(TIMEOUT)))?;
        s.set_write_timeout(Some(Duration::from_secs_f64(TIMEOUT)))
    })();
    if let Err(e) = setup {
        p.shared.die(&p.ev_tx, format!("socket setup failed: {e}"));
        return;
    }
    run_duplex(s, name, p);
}

fn run_duplex<S: Duplex>(s: S, name: String, p: Parts) {
    let Parts { cmd_rx, cmd_tx, ev_tx, out_tx, in_rx, shared } = p;
    let rs = match s.dup() {
        Ok(r) => r,
        Err(e) => {
            shared.die(&ev_tx, format!("socket setup failed: {e}"));
            return;
        }
    };
    let (ev2, sh2) = (ev_tx.clone(), shared.clone());
    let _ = std::thread::Builder::new().name("net-read".into()).spawn(move || reader(rs, cmd_tx, ev2, in_rx, sh2));
    writer(s, name, cmd_rx, ev_tx, out_tx, shared);
}

fn writer<S: Duplex>(mut s: S, name: String, rx: Receiver<Cmd>, ev: Sender<Event>, pool: Sender<Vec<u8>>, sh: Arc<Shared>) {
    let mut scratch: Vec<u8> = Vec::with_capacity(CHUNK + 64);
    let mut chunk = vec![0u8; CHUNK];
    let mut song: Option<(File, u64, u64)> = None;
    let mut last_report = 0u64;
    let mut pings = 0u32;
    let mut next_ping = 0.0;
    macro_rules! put {
        ($bytes:expr) => {{
            let b: &[u8] = $bytes;
            if let Err(e) = s.write_all(b) {
                sh.die(&ev, format!("connection lost: {e}"));
                s.close();
                return;
            }
            sh.sent.fetch_add(b.len() as u64, Ordering::Relaxed);
        }};
    }
    scratch.clear();
    encode(&Msg::Hello { ver: PROTO_VER, name }, &mut scratch);
    put!(&scratch);
    loop {
        let wait = if song.is_some() { 0.0 } else { (next_ping - now()).max(0.0) };
        let cmd = if wait <= 0.0 {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Cmd::Close),
            }
        } else {
            match rx.recv_timeout(Duration::from_secs_f64(wait)) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(Cmd::Close),
            }
        };
        match cmd {
            Some(Cmd::Frame(mut b)) => {
                put!(&b);
                b.clear();
                let _ = pool.send(b);
            }
            Some(Cmd::Pong { t0, th }) => {
                scratch.clear();
                encode(&Msg::Pong { t0, th }, &mut scratch);
                put!(&scratch);
            }
            Some(Cmd::SendSong { path, title, theme, ext }) => {
                song = None;
                let mut size = 0;
                if let Some(p) = &path {
                    match File::open(p).and_then(|f| f.metadata().map(|m| (f, m.len()))) {
                        Ok((_, n)) if n > MAX_SONG => {
                            sh.die(&ev, format!("song is too big to send ({} MB, max {} MB)", n >> 20, MAX_SONG >> 20));
                            s.close();
                            return;
                        }
                        Ok((f, n)) => {
                            size = n;
                            song = Some((f, n, 0));
                        }
                        Err(e) => {
                            sh.die(&ev, format!("cannot read the song: {e}"));
                            s.close();
                            return;
                        }
                    }
                }
                scratch.clear();
                encode(&Msg::Song { title, theme, ext, size }, &mut scratch);
                put!(&scratch);
                last_report = 0;
                let _ = ev.send(Event::SongProgress(0, size));
                if song.is_none() || size == 0 {
                    song = None;
                    let _ = ev.send(Event::SongSent);
                }
            }
            Some(Cmd::Close) => {
                scratch.clear();
                encode(&Msg::Bye, &mut scratch);
                let _ = s.write_all(&scratch);
                let _ = s.flush();
                s.close();
                return;
            }
            None => {}
        }
        if now() >= next_ping {
            scratch.clear();
            encode(&Msg::Ping { t0: now() }, &mut scratch);
            put!(&scratch);
            pings += 1;
            next_ping = now() + if pings < SYNC_SAMPLES as u32 { 0.04 } else { KEEPALIVE };
        }
        let mut finished = false;
        if let Some((f, total, sent)) = &mut song {
            let want = ((*total - *sent) as usize).min(CHUNK);
            match f.read(&mut chunk[..want]) {
                Ok(n) if n > 0 => {
                    scratch.clear();
                    let len = (1 + n) as u32;
                    scratch.extend_from_slice(&len.to_le_bytes());
                    scratch.push(ty::CHUNK);
                    scratch.extend_from_slice(&chunk[..n]);
                    *sent += n as u64;
                    let (done, tot) = (*sent, *total);
                    put!(&scratch);
                    if done - last_report >= 256 * 1024 || done == tot {
                        last_report = done;
                        let _ = ev.send(Event::SongProgress(done, tot));
                    }
                    finished = done >= tot;
                }
                _ => {
                    sh.die(&ev, "the song file changed while sending".into());
                    s.close();
                    return;
                }
            }
        }
        if finished {
            song = None;
            let _ = ev.send(Event::SongSent);
        }
    }
}

/// Where the client stores received songs (fixed names; nothing from the network is a path).
fn song_file(n: u32, ext: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("heartline-lan");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("song_{}_{}.{}", std::process::id(), n, ext))
}

fn reader<S: Duplex>(mut s: S, cmd: Sender<Cmd>, ev: Sender<Event>, pool: Receiver<Vec<u8>>, sh: Arc<Shared>) {
    let mut fr = Framer::new();
    let mut clock = ClockSync::default();
    let mut hello = false;
    // song being received: (file, path, title, size, done)
    let mut song: Option<(File, PathBuf, String, u64, u64)> = None;
    let mut song_n = 0u32;
    let mut last_report = 0u64;
    let mut prev_file: Option<PathBuf> = None;
    // Buffer for the next Chunk/Snap payload (recycled through `pool`).
    let mut spare: Option<Vec<u8>> = None;
    let fail = |why: String, s: &S| {
        sh.die(&ev, why);
        s.close();
    };
    loop {
        if sh.dead.load(Ordering::SeqCst) {
            return;
        }
        let n = match s.read(fr.space()) {
            Ok(0) => return fail("the other player left".into(), &s),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return fail(format!("connection timed out (no data for {TIMEOUT:.0} s)"), &s)
            }
            Err(e) => return fail(format!("connection lost: {e}"), &s),
        };
        sh.recv.fetch_add(n as u64, Ordering::Relaxed);
        fr.filled(n);
        loop {
            let (t, payload) = match fr.next() {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => return fail(format!("bad data from the other player ({e})"), &s),
            };
            if spare.is_none() {
                spare = pool.try_recv().ok();
            }
            let Some(m) = decode(t, payload, &mut spare) else {
                return fail(format!("bad message (type {t}) from the other player"), &s);
            };
            if !hello {
                match m {
                    Msg::Hello { ver, name } if ver == PROTO_VER => {
                        hello = true;
                        let _ = ev.send(Event::Connected(name));
                        continue;
                    }
                    Msg::Hello { ver, .. } => {
                        return fail(format!("the other player is on a different version (protocol v{ver}, you have v{PROTO_VER})"), &s);
                    }
                    _ => return fail("the other side is not Heartline".into(), &s),
                }
            }
            match m {
                Msg::Ping { t0 } => {
                    let _ = cmd.send(Cmd::Pong { t0, th: now() });
                }
                Msg::Pong { t0, th } => {
                    clock.add(t0, th, now());
                    if let Some((offset, rtt)) = clock.best() {
                        let _ = ev.send(Event::Clock { offset, rtt, samples: clock.total });
                    }
                }
                Msg::Bye => return fail("the other player left".into(), &s),
                Msg::Song { title, theme, ext, size } => {
                    song = None;
                    if theme != NO_THEME {
                        let _ = ev.send(Event::SongSaved { path: None, title, theme });
                        continue;
                    }
                    let ext = ext.to_ascii_lowercase();
                    if !crate::decode::AUDIO_EXTS.contains(&ext.as_str()) || size == 0 {
                        return fail(format!("the host sent an unsupported song type '.{ext}'"), &s);
                    }
                    if let Some(p) = prev_file.take() {
                        let _ = std::fs::remove_file(p);
                    }
                    song_n += 1;
                    let path = song_file(song_n, &ext);
                    match File::create(&path) {
                        Ok(f) => song = Some((f, path, title, size, 0)),
                        Err(e) => return fail(format!("cannot save the song: {e}"), &s),
                    }
                    last_report = 0;
                    let _ = ev.send(Event::SongProgress(0, size));
                }
                Msg::Chunk(d) => {
                    let Some((f, _, _, size, done)) = &mut song else {
                        return fail("song data without a song".into(), &s);
                    };
                    *done += d.len() as u64;
                    if *done > *size {
                        return fail("the host sent more song data than announced".into(), &s);
                    }
                    if let Err(e) = f.write_all(&d) {
                        return fail(format!("cannot save the song: {e}"), &s);
                    }
                    let (dn, sz) = (*done, *size);
                    if dn - last_report >= 256 * 1024 || dn == sz {
                        last_report = dn;
                        let _ = ev.send(Event::SongProgress(dn, sz));
                    }
                    spare = Some(d);
                    if dn == sz {
                        let (f, path, title, _, _) = song.take().unwrap();
                        let _ = f.sync_all();
                        drop(f);
                        prev_file = Some(path.clone());
                        let _ = ev.send(Event::SongSaved { path: Some(path), title, theme: NO_THEME });
                    }
                }
                Msg::Hello { .. } => {}
                other => {
                    let _ = ev.send(Event::Msg(other));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct HostInfo {
    pub name: String,
    pub addr: SocketAddr,
    /// Seconds since the last beacon.
    pub age: f32,
    pub compatible: bool,
}

/// Parse a beacon datagram: `HEARTLINE1 <name> <port>`. Other `HEARTLINE<n>` versions parse as
/// incompatible so the list can say so.
pub fn parse_beacon(b: &[u8]) -> Option<(String, u16, bool)> {
    if b.len() > 128 {
        return None;
    }
    let s = std::str::from_utf8(b).ok()?;
    let mut it = s.split_whitespace();
    let magic = it.next()?;
    let name = clean_name(it.next()?);
    let port: u16 = it.next()?.parse().ok()?;
    if it.next().is_some() || port == 0 || !magic.starts_with("HEARTLINE") {
        return None;
    }
    Some((name, port, magic == BEACON_MAGIC))
}

pub struct Discovery {
    rx: Receiver<(String, SocketAddr, bool)>,
    stop: Arc<AtomicBool>,
    pub hosts: Vec<HostInfo>,
    pub error: Option<String>,
}

impl Discovery {
    pub fn start() -> Discovery {
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut error = None;
        match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISC_PORT)) {
            Ok(u) => {
                let _ = u.set_read_timeout(Some(Duration::from_millis(250)));
                let st = stop.clone();
                let _ = std::thread::Builder::new().name("net-disc".into()).spawn(move || {
                    let mut buf = [0u8; 256];
                    while !st.load(Ordering::Relaxed) {
                        if let Ok((n, from)) = u.recv_from(&mut buf) {
                            if let Some((name, port, ok)) = parse_beacon(&buf[..n]) {
                                if tx.send((name, SocketAddr::new(from.ip(), port), ok)).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
            Err(e) => error = Some(format!("cannot listen for hosts ({e}); type the host's IP instead")),
        }
        Discovery { rx, stop, hosts: Vec::new(), error }
    }

    /// Age entries, merge new beacons, forget hosts silent for 5 s.
    pub fn poll(&mut self, dt: f32) {
        for h in &mut self.hosts {
            h.age += dt;
        }
        while let Ok((name, addr, compatible)) = self.rx.try_recv() {
            let n = self.hosts.len();
            match self.hosts.iter_mut().find(|h| h.addr == addr) {
                Some(h) => {
                    h.age = 0.0;
                    h.name = name;
                    h.compatible = compatible;
                }
                None if n < 16 => self.hosts.push(HostInfo { name, addr, age: 0.0, compatible }),
                None => {}
            }
        }
        self.hosts.retain(|h| h.age < 5.0);
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// This PC's LAN address, to show on the host's lobby. A UDP "connect" only picks a route; it
/// sends nothing.
pub fn local_ip() -> Option<std::net::IpAddr> {
    let u = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    u.connect((Ipv4Addr::new(10, 255, 255, 255), 1)).ok()?;
    u.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

/// Parse a typed address: `1.2.3.4` or `1.2.3.4:port`.
pub fn parse_addr(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Some(a);
    }
    s.parse::<std::net::IpAddr>().ok().map(|ip| SocketAddr::new(ip, TCP_PORT))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: Msg) {
        let mut b = Vec::new();
        encode(&m, &mut b);
        let mut fr = Framer::new();
        let sp = fr.space();
        sp[..b.len()].copy_from_slice(&b);
        fr.filled(b.len());
        let (t, p) = fr.next().unwrap().unwrap();
        let got = decode(t, p, &mut None).expect("decodes");
        match (&m, &got) {
            (Msg::Input(a), Msg::Input(b)) => {
                assert_eq!(a.seq, b.seq);
                assert_eq!(a.aim, b.aim);
                assert_eq!(a.swings, b.swings);
                assert!((a.mv[0] - b.mv[0]).abs() < 0.01 && (a.mv[1] - b.mv[1]).abs() < 0.01);
            }
            _ => assert_eq!(m, got),
        }
        assert!(fr.next().unwrap().is_none());
    }

    fn all_msgs() -> Vec<Msg> {
        vec![
            Msg::Hello { ver: PROTO_VER, name: "pc-1".into() },
            Msg::Lobby { ch: 3, ready: true },
            Msg::Song { title: "Ünïcode song".into(), theme: NO_THEME, ext: "mp3".into(), size: 1234 },
            Msg::Song { title: "theme".into(), theme: 2, ext: String::new(), size: 0 },
            Msg::Chunk(vec![7; 1000]),
            Msg::Loaded,
            Msg::Ping { t0: 12.5 },
            Msg::Pong { t0: 12.5, th: 99.25 },
            Msg::Start { host_time: 1e4, seed: 0xDEAD_BEEF_1234, chars: [1, 3] },
            Msg::Input(NetInput { seq: 77, mv: [0.5, -1.0], aim: [123.0, -45.5], swings: 9 }),
            Msg::Snap { t: 3.25, ack: 77, data: vec![1, 2, 3, 4, 5] },
            Msg::End { score: [11, 7] },
            Msg::Bye,
        ]
    }

    #[test]
    fn every_message_roundtrips() {
        for m in all_msgs() {
            roundtrip(m);
        }
    }

    #[test]
    fn snap_fast_path_matches_msg() {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        encode(&Msg::Snap { t: 1.5, ack: 3, data: vec![9; 40] }, &mut a);
        encode_snap(1.5, 3, &[9; 40], &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn malformed_is_rejected() {
        // Every truncation fails (Chunk needs 1 byte of data, Snap its 8-byte header).
        for m in all_msgs() {
            let mut b = Vec::new();
            encode(&m, &mut b);
            let (t, p) = (b[4], &b[5..]);
            let min_ok = match m {
                Msg::Chunk(_) => 1,
                Msg::Snap { .. } => 8,
                _ => p.len(),
            };
            for cut in 0..min_ok {
                assert!(decode(t, &p[..cut], &mut None).is_none(), "{m:?} cut {cut}");
            }
            if !matches!(m, Msg::Chunk(_) | Msg::Snap { .. }) {
                let mut long = p.to_vec();
                long.push(0);
                assert!(decode(t, &long, &mut None).is_none(), "{m:?} + junk");
            }
        }
        assert!(decode(200, &[], &mut None).is_none(), "unknown type");
        assert!(decode(ty::LOBBY, &[1, 2], &mut None).is_none(), "bad bool");
        let mut big = Vec::new();
        encode(&Msg::Song { title: "x".into(), theme: NO_THEME, ext: "mp3".into(), size: 0 }, &mut big);
        let n = big.len();
        big[n - 8..].copy_from_slice(&(MAX_SONG + 1).to_le_bytes());
        assert!(decode(big[4], &big[5..], &mut None).is_none(), "oversized song");
        assert!(decode(ty::PING, &f64::NAN.to_le_bytes(), &mut None).is_none(), "NaN time");
        // Oversized frame length is an error, not an allocation.
        let mut fr = Framer::new();
        let sp = fr.space();
        sp[..5].copy_from_slice(&[0xff, 0xff, 0xff, 0x7f, 1]);
        fr.filled(5);
        assert!(fr.next().is_err());
        let mut fr = Framer::new();
        fr.space()[..4].copy_from_slice(&[0, 0, 0, 0]);
        fr.filled(4);
        assert!(fr.next().is_err(), "zero length");
    }

    #[test]
    fn framer_handles_split_and_merged_reads() {
        let mut stream = Vec::new();
        for m in all_msgs() {
            encode(&m, &mut stream);
        }
        for step in [1usize, 3, 7, 64, 100_000] {
            let mut fr = Framer::new();
            let mut got = 0;
            let mut i = 0;
            while i < stream.len() {
                let n = step.min(stream.len() - i);
                fr.space()[..n].copy_from_slice(&stream[i..i + n]);
                fr.filled(n);
                i += n;
                while let Some((t, p)) = fr.next().unwrap() {
                    assert!(decode(t, p, &mut None).is_some());
                    got += 1;
                }
            }
            assert_eq!(got, all_msgs().len(), "step {step}");
        }
    }

    #[test]
    fn clock_sync_picks_lowest_rtt() {
        // Remote clock is 5.0 s ahead; one-way delays jitter 0.5..20 ms, asymmetric on bad samples.
        let mut rng = crate::rng::Rng::new(3);
        let mut c = ClockSync::default();
        let mut t = 100.0;
        for k in 0..SYNC_SAMPLES {
            let up = 0.0005 + rng.f32() as f64 * 0.02;
            let down = if k == 5 { 0.0006 } else { 0.0005 + rng.f32() as f64 * 0.02 };
            let up = if k == 5 { 0.0005 } else { up };
            let th = t + up + 5.0;
            let t2 = t + up + down;
            c.add(t, th, t2);
            t += 0.05;
        }
        let (off, rtt) = c.best().unwrap();
        assert!(rtt < 0.0012, "rtt {rtt}");
        assert!((off - 5.0).abs() < 0.001, "offset {off}");
    }

    #[test]
    fn beacon_parse() {
        assert_eq!(parse_beacon(b"HEARTLINE1 my-pc 47820"), Some(("my-pc".into(), 47820, true)));
        assert_eq!(parse_beacon(b"HEARTLINE2 my-pc 47820"), Some(("my-pc".into(), 47820, false)));
        assert!(parse_beacon(b"HEARTLINE1 my-pc").is_none());
        assert!(parse_beacon(b"HEARTLINE1 my-pc 0").is_none());
        assert!(parse_beacon(b"OTHER x 1").is_none());
        assert!(parse_beacon(&[0xff; 20]).is_none());
        assert_eq!(parse_addr("10.0.0.5"), Some("10.0.0.5:47820".parse().unwrap()));
        assert_eq!(parse_addr(" 10.0.0.5:9000 "), Some("10.0.0.5:9000".parse().unwrap()));
        assert!(parse_addr("nope").is_none());
    }

    fn wait_for(n: &Net, secs: f64, mut f: impl FnMut(&Event) -> bool) -> bool {
        let end = now() + secs;
        while now() < end {
            while let Some(e) = n.poll() {
                if f(&e) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    /// In-memory duplex pipe with the same timeout / close behaviour as the TCP stream (TCP
    /// connects are not allowed in every test environment).
    struct Mem {
        tx: Sender<Vec<u8>>,
        self_tx: Sender<Vec<u8>>,
        rx: Arc<std::sync::Mutex<(Receiver<Vec<u8>>, Vec<u8>, usize)>>,
    }

    impl Read for Mem {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let mut g = self.rx.lock().unwrap();
            let (rx, buf, pos) = &mut *g;
            if *pos >= buf.len() {
                match rx.recv_timeout(Duration::from_secs_f64(TIMEOUT)) {
                    Ok(v) if v.is_empty() => return Ok(0),
                    Ok(v) => {
                        *buf = v;
                        *pos = 0;
                    }
                    Err(RecvTimeoutError::Timeout) => return Err(ErrorKind::TimedOut.into()),
                    Err(RecvTimeoutError::Disconnected) => return Ok(0),
                }
            }
            let n = out.len().min(buf.len() - *pos);
            out[..n].copy_from_slice(&buf[*pos..*pos + n]);
            *pos += n;
            Ok(n)
        }
    }

    impl Write for Mem {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if b.is_empty() {
                return Ok(0);
            }
            self.tx.send(b.to_vec()).map_err(|_| std::io::Error::from(ErrorKind::BrokenPipe))?;
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Duplex for Mem {
        fn dup(&self) -> std::io::Result<Self> {
            Ok(Mem { tx: self.tx.clone(), self_tx: self.self_tx.clone(), rx: self.rx.clone() })
        }
        fn close(&self) {
            let _ = self.tx.send(Vec::new());
            let _ = self.self_tx.send(Vec::new());
        }
    }

    fn mem_pair() -> (Mem, Mem) {
        let (a_tx, a_rx) = channel();
        let (b_tx, b_rx) = channel();
        let a = Mem { tx: a_tx.clone(), self_tx: b_tx.clone(), rx: Arc::new(std::sync::Mutex::new((b_rx, Vec::new(), 0))) };
        let b = Mem { tx: b_tx, self_tx: a_tx, rx: Arc::new(std::sync::Mutex::new((a_rx, Vec::new(), 0))) };
        (a, b)
    }

    fn mem_net(s: Mem, name: &str) -> Net {
        let (net, parts) = Net::make(0);
        let name = name.to_string();
        std::thread::spawn(move || run_duplex(s, name, parts));
        net
    }

    fn session(host: Net, client: Net) {
        assert!(wait_for(&host, 3.0, |e| matches!(e, Event::Connected(n) if n == "client")));
        assert!(wait_for(&client, 3.0, |e| matches!(e, Event::Connected(n) if n == "host")));
        // clock: same process, so the offset must be ~0
        let mut off = 1.0;
        assert!(wait_for(&client, 3.0, |e| {
            if let Event::Clock { offset, samples, .. } = e {
                off = *offset;
                *samples >= SYNC_SAMPLES as u32
            } else {
                false
            }
        }));
        assert!(off.abs() < 0.005, "offset {off}");
        // song transfer (several chunks, odd final size)
        let dir = std::env::temp_dir().join(format!("heartline-net-test-{}-{}", std::process::id(), host.port));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.wav");
        let data: Vec<u8> = (0..300_001u32).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        host.send_song(Some(src), "Test Song", NO_THEME, "wav");
        let mut saved = None;
        let mut last = (0, 0);
        assert!(wait_for(&client, 5.0, |e| match e {
            Event::SongProgress(d, t) => {
                last = (*d, *t);
                false
            }
            Event::SongSaved { path, title, .. } => {
                assert_eq!(title, "Test Song");
                saved = path.clone();
                true
            }
            _ => false,
        }));
        assert_eq!(last, (data.len() as u64, data.len() as u64));
        let path = saved.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert!(path.starts_with(std::env::temp_dir()));
        // built-in theme: no bytes
        host.send_song(None, "Theme", 1, "");
        assert!(wait_for(&client, 2.0, |e| matches!(e, Event::SongSaved { path: None, theme: 1, .. })));
        // input + snapshots, buffers recycled
        client.send(&Msg::Input(NetInput { seq: 5, mv: [1.0, 0.0], aim: [1.0, 2.0], swings: 1 }));
        assert!(wait_for(&host, 2.0, |e| matches!(e, Event::Msg(Msg::Input(i)) if i.seq == 5)));
        for k in 0..100u32 {
            host.send_snap(k as f32, k, &[k as u8; 40]);
        }
        let mut got = 0u32;
        assert!(wait_for(&client, 2.0, |e| {
            if let Event::Msg(Msg::Snap { ack, data, .. }) = e {
                assert_eq!(*ack, got, "in order");
                assert_eq!(data.len(), 40);
                got += 1;
            }
            got == 100
        }));
        // host leaves: the client gets a clean disconnect
        drop(host);
        let mut why = String::new();
        assert!(wait_for(&client, 2.0, |e| {
            if let Event::Disconnected(w) = e {
                why = w.clone();
                true
            } else {
                false
            }
        }));
        assert!(why.contains("left"), "{why}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mem_session() {
        let (a, b) = mem_pair();
        session(mem_net(a, "host"), mem_net(b, "client"));
    }

    #[test]
    #[ignore = "needs loopback TCP; run with --ignored where sockets are allowed"]
    fn tcp_session() {
        let host = Net::host("host", 0, false).expect("bind");
        let addr: SocketAddr = format!("127.0.0.1:{}", host.port).parse().unwrap();
        let client = Net::join(addr, "client");
        session(host, client);
    }

    fn disconnect_reason(n: &Net, secs: f64) -> Option<String> {
        let mut why = None;
        wait_for(n, secs, |e| {
            if let Event::Disconnected(w) = e {
                why = Some(w.clone());
                true
            } else {
                false
            }
        });
        why
    }

    #[test]
    fn version_mismatch_and_garbage_are_rejected() {
        let (a, mut raw) = mem_pair();
        let host = mem_net(a, "host");
        let mut b = Vec::new();
        encode(&Msg::Hello { ver: PROTO_VER + 1, name: "future".into() }, &mut b);
        raw.write_all(&b).unwrap();
        let why = disconnect_reason(&host, 3.0).unwrap();
        assert!(why.contains("different version"), "{why}");

        let (a, mut raw) = mem_pair();
        let host = mem_net(a, "host");
        raw.write_all(&[0xff, 0xff, 0xff, 0xff, 1, 2, 3]).unwrap();
        assert!(disconnect_reason(&host, 3.0).unwrap().contains("bad data"));

        // valid hello, then an unknown message type
        let (a, mut raw) = mem_pair();
        let host = mem_net(a, "host");
        let mut b = Vec::new();
        encode(&Msg::Hello { ver: PROTO_VER, name: "x".into() }, &mut b);
        b.extend_from_slice(&[1, 0, 0, 0, 99]);
        raw.write_all(&b).unwrap();
        assert!(disconnect_reason(&host, 3.0).unwrap().contains("bad message"));

        // song data that would escape the temp dir is impossible: the extension is whitelisted
        let (a, mut raw) = mem_pair();
        let client = mem_net(a, "client");
        let mut b = Vec::new();
        encode(&Msg::Hello { ver: PROTO_VER, name: "x".into() }, &mut b);
        encode(&Msg::Song { title: "x".into(), theme: NO_THEME, ext: "/../a".into(), size: 10 }, &mut b);
        raw.write_all(&b).unwrap();
        assert!(disconnect_reason(&client, 3.0).unwrap().contains("unsupported"));
    }

    #[test]
    fn silent_peer_times_out() {
        let (a, mut raw) = mem_pair();
        let host = mem_net(a, "host");
        let mut b = Vec::new();
        encode(&Msg::Hello { ver: PROTO_VER, name: "mute".into() }, &mut b);
        raw.write_all(&b).unwrap();
        assert!(wait_for(&host, 2.0, |e| matches!(e, Event::Connected(_))));
        let t0 = now();
        let why = disconnect_reason(&host, 5.0).unwrap_or_default();
        let waited = now() - t0;
        assert!(why.contains("timed out") && (2.5..4.5).contains(&waited), "{why} after {waited}");
        drop(raw);
    }
}
