# LAN + Rally mode

Goal: two PCs on the same network play **Rally**, a beat-locked paddle duel, on a song the host
picks. The song's beats, sections and visual layer drive everything. Later LAN modes (Duet co-op,
Heart vs Runner, Crossfire) reuse the same network layer.

## Architecture
- **Host-authoritative.** The host runs the Rally simulation. The client sends its input and the
  host sends state snapshots. The client interpolates and predicts its own avatar.
- **The song travels as a file,** before the match, over the same TCP connection (cap 200 MB).
  Both PCs analyse it locally (analysis is deterministic, so they get the same beats) and both
  play their own copy, started at an agreed clock time. There's no audio streaming.
- **Transport:** `std::net` only (no new crates).
  - **TCP** with `TCP_NODELAY` for everything during the match: on a LAN the round-trip is about
    1 ms, which is fine, and it removes a reliability layer for v1.
  - **UDP broadcast** only for discovery.
  - Manual IP entry as a fallback.
- **Threads:** a net thread owns the sockets and talks to the main thread through channels
  (`try_recv` each frame). The main thread never blocks.

## Files and ownership
| File | Owner | Contents |
|---|---|---|
| `src/net.rs` (new) | network-programmer | discovery, connection, protocol, clock sync, file transfer, net thread |
| `src/main.rs` | network-programmer | menu entry, lobby screens, match flow, wiring Rally + net + audio |
| `src/rally.rs` (new) | gameplay-programmer | Rally rules and simulation, characters, bot, snapshot encode/decode, tests |
| `src/render.rs` | gameplay-programmer | `Renderer::draw_rally`, reusing the background and visual layer |

## Interface between them (`rally.rs`)
```
pub enum Character { Sub, Crack, Tick, Arc }          // bass, snare, hat, lead
pub struct RallyInput { pub mv: Vec2, pub aim: Vec2, pub swing: bool }   // mv in -1..1
pub struct RallyMusic<'a> { pub beats: &'a [f32], pub bars: &'a [f32],
                            pub section: SectionKind, pub next_build: f32 /*0..1*/ }
pub struct Rally { .. }                                // full state; Clone
impl Rally {
    pub fn new(seed: u64, chars: [Character; 2]) -> Rally;
    pub fn step(&mut self, dt: f32, t: f32, inp: [RallyInput; 2], m: &RallyMusic);   // host
    pub fn encode(&self, out: &mut Vec<u8>);            // snapshot, < 512 bytes
    pub fn decode(&mut self, b: &[u8]) -> bool;         // client; false if malformed
    pub fn interpolate(&mut self, a: &Rally, b: &Rally, k: f32);   // client smoothing
    pub fn score(&self) -> [u32; 2];
    pub fn bot_input(&self, me: usize, t: f32, m: &RallyMusic) -> RallyInput;
}
// render.rs
impl Renderer { pub fn draw_rally(&mut self, r: &Rally, music: &World, me: usize, dt: f32); }
```
`music: &World` is a World fed with `feat`, `hit`, `bar_pulse` etc. as usual but with no level
events. It drives the existing visual layer. Player 0 = host (left), 1 = client (right).

## Rally rules (v1)
- **Arena:** the existing arena. Each player owns an end zone (the outer 25% of the width) and
  moves freely in 2D inside it. Their goal is the end line behind them.
- **Swing:** click or Space opens a short hit arc (about 120 ms) facing the aim, then a cooldown.
  If the orb touches the arc, it returns toward the aim direction.
- **Beat lock:** on every return, the orb's speed is set so it reaches the opponent's end line
  exactly on a beat: the first beat at or after `now + min_flight`.
  - **Perfect:** a swing within ±70 ms of a beat is a perfect return. It arrives one beat sooner
    and triggers the character's power.
- **Characters** (powers trigger on perfect returns):
  - **Sub** (bass): heavy slam that knocks the opponent back on contact, with a floor shockwave.
  - **Crack** (snare): the orb splits into two for that volley.
  - **Tick** (hat): the orb zig-zags, flipping its sideways direction each half-beat.
  - **Arc** (lead): the orb curves in flight and leaves a laser trail.
- **The music changes the match:**
  - `min_flight` is Calm 3 beats, Normal 2, Intense 1.5, Drop 1.
  - During a build (`next_build` rising), the goals widen (more pressure).
  - A Drop section adds a second orb for its duration.
- **Scoring:** an orb crossing your end line gives the opponent a point. The player who conceded
  serves on the next downbeat (the orb appears in front of them). Most points at song end wins.
- **Visualizer as the field:**
  - the orb trail is the live waveform
  - each avatar carries a mini spectrum in its colour
  - the end lines are ridgelines that rise as an orb approaches
  - the Heart sits in the centre as a non-interactive pulsing centrepiece

## Network flow
1. Menu → `LAN RALLY` → **Host** or **Join**.
2. **Host** picks a song (current track or a file dialog), opens TCP port 47820, and broadcasts
   UDP `HEARTLINE1 <name> <port>` on 47821 every second.
3. **Join** lists the hosts it hears, or takes a typed IP. It connects and sends `Hello{ver, name}`.
4. **Lobby:** both pick a character with left/right and press Ready.
   - The host sends `Song{title, bytes}` with a progress bar on both sides.
   - The client saves it to the OS temp dir, analyses it, and replies `Loaded`.
5. **Clock sync:** 8 ping/pongs. `offset` = the pong with the lowest round-trip.
6. The host sends `Start{host_time + 3 s, seed, chars}`. Both show a countdown and start playback
   at that instant on their own clock.
7. **Match:**
   - The client sends `Input` at 60 Hz.
   - The host steps the sim and sends `Snap` at 60 Hz.
   - The client renders 1 snapshot behind (interpolated) and moves its own avatar at once
     (prediction).
8. Song end → results (score, winner) → back to the lobby (same connection, the host picks next).
9. Any disconnect or timeout (3 s without data) → menu with a clear message.

**Protocol:** `[u32 len][u8 type][payload]`, little-endian, version byte in `Hello`. Every length
and enum is bounds-checked, and any allocation from network data is capped.

## Verification
- `cargo test --release`: the Rally sim tests (the orb lands within 5 ms of a beat; scoring;
  encode/decode round-trip; malformed decode is rejected).
- **Two instances on one PC:** host on `127.0.0.1`. CLI for bots:
  `--lan-host FILE --rally-bot` and `--lan-join 127.0.0.1 --rally-bot`, combined with
  `--selftest DIR --shots ... --secs N`. Both logs must show the same final score and no
  disconnect.
- Windows Firewall will ask the first time the host listens. The user allows "Private networks".

## Later
Duet co-op, Heart vs Runner, Crossfire, best-of-N song matches, a spectator view.

## Progress (2026-11-14)
- **Built:**
  - `src/net.rs` (discovery, TCP, framing, clock sync, song transfer, net threads)
  - `src/rally.rs` (rules, characters, bot, snapshots)
  - `Renderer::draw_rally`
  - main.rs wiring: menu row `LAN RALLY`, Host/Join, lobby, countdown, match, results, and the
    flags `--lan-host` / `--lan-join` / `--rally-bot`
- **Tests:** 27 pass and 1 is ignored (the real-TCP test), including in-memory session tests for
  sync, the song transfer and snapshots.
- **Not verified yet:** a real two-instance match, and Rally on screen. Every loopback TCP connect
  fails with WSAEACCES when started from inside the desktop coding app (even its terminal panel).
  The host listens fine (`0.0.0.0:47820`).
  - Test from a normal PowerShell window outside that app:
    `powershell -ExecutionPolicy Bypass -File tmp\lan-test.ps1`
  - Pass = both `tmp\h\lan.log` and `tmp\c\lan.log` end with the same final score and show no
    `leave:` line.
- **Next:** fix whatever that run shows. Then add an offline `--rally-local` (vs bot) mode so
  Rally can be screenshotted and practised without a network.
