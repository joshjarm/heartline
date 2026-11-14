//! Rally: a beat-locked paddle duel for LAN play (docs/plans/lan-rally.md).
//!
//! Each player owns an end zone (outer 25% of the arena width) and defends the goal on the end
//! line behind them. A swing opens a short arc facing the aim; an orb touching it is returned
//! toward the aim with its speed set so it reaches the far end line exactly on a beat (the first
//! beat at or after `now + min_flight`). A swing within `PERFECT_WIN` of a beat is a perfect
//! return: it lands one beat sooner and fires the character's power.
//!
//! Orb flight is parametric (start, launch time, arrival beat), so positions are exact functions
//! of the song time: arrival is on the beat by construction, and the client can evaluate trails.
//! Deterministic from the seed + inputs: no wall clock, no HashMap, randomness via `rng.rs`
//! keyed on (seed, orb id), so a decoded snapshot continues identically.

use macroquad::math::{vec2, Vec2};
use std::f32::consts::PI;

use crate::analysis::SectionKind;
use crate::level::{HALF_H, HALF_W};
use crate::rng::Rng;

// ---- arena / avatars ----
pub const ZONE_W: f32 = HALF_W * 0.5; // each end zone: the outer 25% of the full width
pub const AVATAR_R: f32 = 14.0;
pub const ORB_R: f32 = 12.0;
pub const PLAYER_SPEED: f32 = 600.0; // px/s
const KNOCK_DECAY: f32 = 6.0; // 1/s, knock-back velocity decay
const MAX_DT: f32 = 0.1;
// ---- swing ----
pub const SWING_ACTIVE: f32 = 0.12; // s, the arc is live this long after the press
pub const SWING_CD: f32 = 0.30; // s, from press to the next allowed swing
pub const SWING_R: f32 = 86.0; // px, arc reach from the avatar centre
pub const SWING_HALF: f32 = 1.2; // rad, arc half-angle around the aim (~69 deg)
pub const PERFECT_WIN: f32 = 0.100; // s, |contact - strike beat| for a perfect return
const RET_MAX_ANGLE: f32 = 1.05; // rad, returns are clamped to +-60 deg off straight ahead
// ---- beat-locked flight ----
pub const STRIKE_IN: f32 = 125.0; // px, strike line in front of each end line: orbs cross it on a beat
/// `Fx::err` when the event has no timing judgement.
pub const NO_ERR: i16 = i16::MIN;
const MIN_FLIGHT_CALM: f32 = 3.0; // beats
const MIN_FLIGHT_NORMAL: f32 = 2.0;
const MIN_FLIGHT_INTENSE: f32 = 1.5;
const MIN_FLIGHT_DROP: f32 = 1.0;
const MIN_FLIGHT_S: f32 = 0.38; // s, absolute floor so fast songs stay reactable
const BEAT_EPS: f32 = 1e-4; // s, a target that lands on a beat counts as that beat
const DEFAULT_BEAT: f32 = 0.5; // s, beat grid used when the song has fewer than 2 beats
const DEFAULT_BAR: f32 = 2.0; // s, bar grid used when the song has fewer than 2 bars
// ---- powers (perfect returns) ----
const SUB_KNOCK: f32 = 950.0; // px/s knock-back impulse on contact with a Sub slam
const SUB_STUN: f32 = 0.30; // s, swing lock-out after being slammed
const CRACK_SPREAD: f32 = 210.0; // px, the split orbs land this far either side of the aim line
pub const ZIG_AMP: f32 = 62.0; // px, Tick zig-zag amplitude (flips every half beat)
const ZIG_RAMP: f32 = 5.0; // zig-zag fades in over the first 1/ZIG_RAMP of the flight
pub const CURVE_AMP: f32 = 230.0; // px, Arc mid-flight bulge
// ---- goals (build-ups widen them) ----
pub const GOAL_HALF: f32 = 250.0; // px, goal half-height normally
pub const GOAL_HALF_BUILD: f32 = 390.0; // px, at the peak of a build (`next_build` = 1)
// ---- serve ----
const SERVE_GAP: f32 = 0.6; // s, earliest serve after a point (then the next downbeat)
const SERVE_DIST: f32 = 64.0; // px in front of the server
// ---- capacity ----
pub const MAX_ORBS: usize = 4;
pub const MAX_FX: usize = 8;
const NEVER: f32 = -1.0e6;
// ---- bot ----
const BOT_REACT: f32 = 0.14; // s before the bot reacts to a new return
const BOT_HOME_X: f32 = 0.45; // idle position, as a fraction of the zone width from the end line
const BOT_STANDOFF: f32 = 40.0; // px the bot stands behind the strike line
const BOT_GAIN: f32 = 0.10; // s, movement look-ahead (smaller = snappier)
const BOT_MISS: f32 = 0.22; // chance per incoming orb that the bot misjudges it
const BOT_MISS_OFF: f32 = 150.0; // px lateral error on a misjudged orb
const BOT_EARLY: f32 = 0.09; // s, the bot presses up to this early...
const BOT_LATE: f32 = 0.06; // s, ...or this late around the strike beat (late ones can whiff)
const BOT_AIM_GOAL: f32 = 0.7; // aims at this fraction of the goal half-height
const BOT_SERVE_WAIT: f32 = 0.3; // s before auto-serve at which the bot stops waiting for a beat

const SNAP_VER: u8 = 0xA2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Character {
    Sub,
    Crack,
    Tick,
    Arc,
}

impl Character {
    pub const ALL: [Character; 4] = [Character::Sub, Character::Crack, Character::Tick, Character::Arc];

    pub fn name(self) -> &'static str {
        match self {
            Character::Sub => "SUB",
            Character::Crack => "CRACK",
            Character::Tick => "TICK",
            Character::Arc => "ARC",
        }
    }

    pub fn index(self) -> u8 {
        self as u8
    }

    pub fn from_index(i: u8) -> Option<Character> {
        Character::ALL.get(i as usize).copied()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RallyInput {
    /// Movement, each axis in -1..1.
    pub mv: Vec2,
    /// Aim direction from the avatar (any length; zero keeps the previous aim).
    pub aim: Vec2,
    /// Swing button held. Swings start on the rising edge.
    pub swing: bool,
}

pub struct RallyMusic<'a> {
    pub beats: &'a [f32],
    pub bars: &'a [f32],
    pub section: SectionKind,
    /// 0..1, rising through a build-up.
    pub next_build: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    None,
    Sub,
    Tick,
    Arc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrbState {
    Dead,
    /// Waiting in front of the server (`owner`); auto-launches at `t1`.
    Held,
    Flying,
}

#[derive(Clone, Copy, Debug)]
pub struct Orb {
    pub id: u16,
    pub state: OrbState,
    /// A Crack split orb: lives for one volley only.
    pub extra: bool,
    /// Last hitter (attacker), or the server while held.
    pub owner: u8,
    pub power: Power,
    pub p0: Vec2,
    pub t0: f32,
    /// Arrival time at the end line (just after `tb`), or the auto-serve time while held.
    pub t1: f32,
    /// The beat on which the orb crosses the defender's strike line.
    pub tb: f32,
    pub xe: f32,
    /// Unfolded aim-line y at the end line.
    pub y1: f32,
    pub curve: f32,
    pub blen: f32,
    /// Folded landing y on the end line (the telegraph).
    pub land: f32,
    pub pos: Vec2,
}

impl Orb {
    const DEAD: Orb = Orb {
        id: 0,
        state: OrbState::Dead,
        extra: false,
        owner: 0,
        power: Power::None,
        p0: Vec2::ZERO,
        t0: 0.0,
        t1: 0.0,
        tb: 0.0,
        xe: 0.0,
        y1: 0.0,
        curve: 0.0,
        blen: DEFAULT_BEAT,
        land: 0.0,
        pos: Vec2::ZERO,
    };

    /// Position at song time `t` (exact; flying orbs only move along their beat-locked path).
    pub fn eval(&self, t: f32) -> Vec2 {
        if self.state != OrbState::Flying {
            return self.pos;
        }
        let s = self.progress(t);
        let x = self.p0.x + (self.xe - self.p0.x) * s;
        let mut y = self.p0.y + (self.y1 - self.p0.y) * s;
        match self.power {
            Power::Tick => {
                // zero on every beat (so at the strike line) and faded out by the end line
                let f = ((self.tb - t) / self.blen.max(0.05)).rem_euclid(1.0);
                y += ZIG_AMP * tri(f) * (s * ZIG_RAMP).min(1.0) * ((1.0 - s) * ZIG_RAMP).min(1.0);
            }
            Power::Arc => y += CURVE_AMP * self.curve * (PI * s).sin(),
            _ => {}
        }
        vec2(x, fold(y))
    }

    /// 0 at launch .. 1 at the end line.
    pub fn progress(&self, t: f32) -> f32 {
        ((t - self.t0) / (self.t1 - self.t0).max(1e-3)).clamp(0.0, 1.0)
    }

    /// Song time at which the orb is at x (its x is linear in time).
    pub fn time_at_x(&self, x: f32) -> f32 {
        let span = self.xe - self.p0.x;
        if span.abs() < 1e-3 {
            return self.t1;
        }
        self.t0 + (x - self.p0.x) / span * (self.t1 - self.t0)
    }

    /// Where the orb crosses the defender's strike line (on beat `tb`).
    pub fn strike_point(&self) -> Vec2 {
        self.eval(self.tb)
    }

    /// The player this orb is heading for.
    pub fn target(&self) -> usize {
        1 - (self.owner as usize & 1)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Player {
    pub pos: Vec2,
    pub aim: Vec2,
    pub knock: Vec2,
    /// Song time of the last swing press.
    pub swing_t: f32,
    pub perfect: bool,
    pub cd: f32,
    pub prev: bool,
    pub ch: Character,
}

impl Player {
    pub fn swing_age(&self, t: f32) -> f32 {
        t - self.swing_t
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxKind {
    Hit,
    Perfect,
    Slam,
    Split,
    Score,
    Serve,
    Bounce,
}

const FX_KINDS: [FxKind; 7] = [FxKind::Hit, FxKind::Perfect, FxKind::Slam, FxKind::Split, FxKind::Score, FxKind::Serve, FxKind::Bounce];

/// A one-shot event for render / audio hooks. `t` is the song time it happened (`NEVER` = empty).
#[derive(Clone, Copy, Debug)]
pub struct Fx {
    pub kind: FxKind,
    pub who: u8,
    pub pos: Vec2,
    pub t: f32,
    /// Contact timing for Hit / Perfect: ms vs the strike beat (negative = early), or `NO_ERR`.
    pub err: i16,
}

#[derive(Clone, Debug)]
pub struct Rally {
    pub t: f32,
    pub players: [Player; 2],
    pub orbs: [Orb; MAX_ORBS],
    pub fx: [Fx; MAX_FX],
    fx_head: u8,
    pub scores: [u32; 2],
    pub goal_half: f32,
    pub section: SectionKind,
    pub serve_pending: bool,
    pub serve_at: f32,
    pub server: u8,
    pub last_conceder: u8,
    next_id: u16,
    seed: u64,
}

// ---------------------------------------------------------------------------------------------
// Geometry and beat-grid helpers
// ---------------------------------------------------------------------------------------------

/// +1 for player 0 (shoots right), -1 for player 1.
pub fn fwd(i: usize) -> f32 {
    if i == 0 {
        1.0
    } else {
        -1.0
    }
}

/// x of player `i`'s own end line (the goal they defend).
pub fn end_x(i: usize) -> f32 {
    -fwd(i) * HALF_W
}

/// x of player `i`'s strike line: incoming orbs cross it exactly on a beat.
pub fn strike_x(i: usize) -> f32 {
    end_x(i) + fwd(i) * STRIKE_IN
}

fn zone_clamp(i: usize, p: Vec2) -> Vec2 {
    let (a, b) = (end_x(i), end_x(i) + fwd(i) * ZONE_W);
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let ylim = HALF_H - AVATAR_R;
    vec2(p.x.clamp(lo + AVATAR_R, hi - AVATAR_R), p.y.clamp(-ylim, ylim))
}

/// Reflect an unbounded y into the arena (the orb bounces off the side walls).
pub fn fold(y: f32) -> f32 {
    let lim = HALF_H - ORB_R;
    let span = 2.0 * lim;
    let u = (y + lim).rem_euclid(2.0 * span);
    let u = if u > span { 2.0 * span - u } else { u };
    u - lim
}

/// Triangle wave, 0 at f = 0 and 0.5, +1 at 0.25, -1 at 0.75: direction flips every half beat.
fn tri(f: f32) -> f32 {
    if f < 0.25 {
        4.0 * f
    } else if f < 0.75 {
        2.0 - 4.0 * f
    } else {
        4.0 * f - 4.0
    }
}

fn closest_on_seg(a: Vec2, b: Vec2, p: Vec2) -> Vec2 {
    let d = b - a;
    let l2 = d.length_squared();
    if l2 < 1e-6 {
        return a;
    }
    a + d * ((p - a).dot(d) / l2).clamp(0.0, 1.0)
}

fn grid_step(g: &[f32], first: bool, def: f32) -> f32 {
    let n = g.len();
    let s = if n < 2 {
        def
    } else if first {
        g[1] - g[0]
    } else {
        g[n - 1] - g[n - 2]
    };
    if s.is_finite() && s > 0.05 {
        s
    } else {
        def
    }
}

/// Time of grid point `i`, extrapolated past either end with the edge spacing.
pub fn grid_time(g: &[f32], i: i64, def: f32) -> f32 {
    let n = g.len() as i64;
    if n == 0 {
        return i as f32 * def;
    }
    if i < 0 {
        g[0] + i as f32 * grid_step(g, true, def)
    } else if i >= n {
        g[(n - 1) as usize] + (i - n + 1) as f32 * grid_step(g, false, def)
    } else {
        g[i as usize]
    }
}

/// Index of the first grid point at or after `x`.
pub fn grid_after(g: &[f32], x: f32, def: f32) -> i64 {
    let n = g.len();
    if n == 0 {
        return (x / def).ceil() as i64;
    }
    if x < g[0] {
        return -(((g[0] - x) / grid_step(g, true, def)).floor() as i64);
    }
    let last = g[n - 1];
    if x > last {
        return (n as i64 - 1) + ((x - last) / grid_step(g, false, def)).ceil() as i64;
    }
    g.partition_point(|&b| b < x) as i64
}

/// Distance in seconds from `t` to the nearest beat.
pub fn beat_dist(beats: &[f32], t: f32) -> f32 {
    let i = grid_after(beats, t, DEFAULT_BEAT);
    (grid_time(beats, i, DEFAULT_BEAT) - t).abs().min((t - grid_time(beats, i - 1, DEFAULT_BEAT)).abs())
}

/// Signed offset in seconds from the nearest beat to `t` (negative = before the beat).
pub fn beat_off(beats: &[f32], t: f32) -> f32 {
    let i = grid_after(beats, t, DEFAULT_BEAT);
    let (a, b) = (t - grid_time(beats, i, DEFAULT_BEAT), t - grid_time(beats, i - 1, DEFAULT_BEAT));
    if a.abs() < b.abs() {
        a
    } else {
        b
    }
}

fn err_ms(e: f32) -> i16 {
    if e.is_finite() {
        (e * 1000.0).round().clamp(-30000.0, 30000.0) as i16
    } else {
        NO_ERR
    }
}

fn beat_len_at(beats: &[f32], t: f32) -> f32 {
    let i = grid_after(beats, t, DEFAULT_BEAT);
    let l = grid_time(beats, i, DEFAULT_BEAT) - grid_time(beats, i - 1, DEFAULT_BEAT);
    if l.is_finite() && l > 0.05 {
        l
    } else {
        DEFAULT_BEAT
    }
}

pub fn min_flight(k: SectionKind) -> f32 {
    match k {
        SectionKind::Calm => MIN_FLIGHT_CALM,
        SectionKind::Normal => MIN_FLIGHT_NORMAL,
        SectionKind::Intense => MIN_FLIGHT_INTENSE,
        SectionKind::Drop => MIN_FLIGHT_DROP,
    }
}

/// Beat-locked arrival at the defender's strike line for a return at `t`: the first beat at or
/// after `t + min_flight` (one beat sooner on a perfect, never under `MIN_FLIGHT_S`).
pub fn arrival_time(m: &RallyMusic, t: f32, perfect: bool) -> f32 {
    let b = beat_len_at(m.beats, t);
    let earliest = t + MIN_FLIGHT_S;
    let mut i = grid_after(m.beats, (t + min_flight(m.section) * b).max(earliest) - BEAT_EPS, DEFAULT_BEAT);
    if perfect && grid_time(m.beats, i - 1, DEFAULT_BEAT) >= earliest {
        i -= 1;
    }
    grid_time(m.beats, i, DEFAULT_BEAT)
}

fn h01(seed: u64, key: u64) -> f32 {
    Rng::new(seed ^ key.wrapping_mul(0xD1B5_4A32_D192_ED03)).f32()
}

fn finite(v: Vec2) -> bool {
    v.x.is_finite() && v.y.is_finite()
}

fn section_index(k: SectionKind) -> u8 {
    match k {
        SectionKind::Calm => 0,
        SectionKind::Normal => 1,
        SectionKind::Intense => 2,
        SectionKind::Drop => 3,
    }
}

// ---------------------------------------------------------------------------------------------
// Simulation
// ---------------------------------------------------------------------------------------------

impl Rally {
    pub fn new(seed: u64, chars: [Character; 2]) -> Rally {
        let player = |i: usize| Player {
            pos: vec2(end_x(i) + fwd(i) * ZONE_W * BOT_HOME_X, 0.0),
            aim: vec2(fwd(i), 0.0),
            knock: Vec2::ZERO,
            swing_t: NEVER,
            perfect: false,
            cd: 0.0,
            prev: false,
            ch: chars[i],
        };
        Rally {
            t: 0.0,
            players: [player(0), player(1)],
            orbs: [Orb::DEAD; MAX_ORBS],
            fx: [Fx { kind: FxKind::Hit, who: 0, pos: Vec2::ZERO, t: NEVER, err: NO_ERR }; MAX_FX],
            fx_head: 0,
            scores: [0; 2],
            goal_half: GOAL_HALF,
            section: SectionKind::Normal,
            serve_pending: false,
            serve_at: 0.0,
            server: 0,
            last_conceder: Rng::new(seed).below(2) as u8,
            next_id: 1,
            seed,
        }
    }

    pub fn score(&self) -> [u32; 2] {
        self.scores
    }

    /// Fx events in no particular order (skip ones with `t == NEVER`; age = `self.t - fx.t`).
    pub fn events(&self) -> impl Iterator<Item = &Fx> {
        self.fx.iter().filter(|f| f.t > NEVER * 0.5)
    }

    fn push_fx(&mut self, kind: FxKind, who: usize, pos: Vec2) {
        self.push_timed(kind, who, pos, NO_ERR);
    }

    fn push_timed(&mut self, kind: FxKind, who: usize, pos: Vec2, err: i16) {
        let i = self.fx_head as usize % MAX_FX;
        self.fx[i] = Fx { kind, who: who as u8, pos, t: self.t, err };
        self.fx_head = ((i + 1) % MAX_FX) as u8;
    }

    fn new_id(&mut self) -> u16 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Host: advance by `dt` to song time `t`.
    pub fn step(&mut self, dt: f32, t: f32, inp: [RallyInput; 2], m: &RallyMusic) {
        let dt = if dt.is_finite() { dt.clamp(0.0, MAX_DT) } else { 0.0 };
        if !t.is_finite() {
            return;
        }
        self.t = t;
        self.section = m.section;
        let nb = if m.next_build.is_finite() { m.next_build.clamp(0.0, 1.0) } else { 0.0 };
        self.goal_half = GOAL_HALF + (GOAL_HALF_BUILD - GOAL_HALF) * nb;
        for i in 0..2 {
            self.step_player(i, dt, t, &inp[i]);
        }
        self.step_serve(t, m);
        for k in 0..MAX_ORBS {
            if self.orbs[k].state != OrbState::Dead {
                self.step_orb(k, t, m);
            }
        }
    }

    fn step_player(&mut self, i: usize, dt: f32, t: f32, inp: &RallyInput) {
        let p = &mut self.players[i];
        if finite(inp.aim) && inp.aim.length_squared() > 1e-4 {
            p.aim = inp.aim.normalize();
        }
        let mv = if finite(inp.mv) { inp.mv.clamp_length_max(1.0) } else { Vec2::ZERO };
        p.knock *= (-dt * KNOCK_DECAY).exp();
        p.pos = zone_clamp(i, p.pos + (mv * PLAYER_SPEED + p.knock) * dt);
        p.cd = (p.cd - dt).max(0.0);
        if inp.swing && !p.prev && p.cd <= 0.0 {
            p.swing_t = t;
            p.cd = SWING_CD;
            p.perfect = false; // judged at contact
        }
        p.prev = inp.swing;
    }

    /// Client prediction: move avatar `i` (aim + movement, zone-clamped) without touching swings
    /// or orbs. Apply after `decode`/`interpolate` so the local avatar responds at once.
    pub fn predict_move(&mut self, i: usize, dt: f32, inp: &RallyInput) {
        let i = i & 1;
        let dt = if dt.is_finite() { dt.clamp(0.0, MAX_DT) } else { 0.0 };
        let p = &mut self.players[i];
        if finite(inp.aim) && inp.aim.length_squared() > 1e-4 {
            p.aim = inp.aim.normalize();
        }
        let mv = if finite(inp.mv) { inp.mv.clamp_length_max(1.0) } else { Vec2::ZERO };
        p.pos = zone_clamp(i, p.pos + (mv * PLAYER_SPEED + p.knock) * dt);
    }

    fn live_main_orbs(&self) -> usize {
        self.orbs.iter().filter(|o| o.state != OrbState::Dead && !o.extra).count()
    }

    fn step_serve(&mut self, t: f32, m: &RallyMusic) {
        let required = if m.section == SectionKind::Drop { 2 } else { 1 };
        let live = self.live_main_orbs();
        if !self.serve_pending && live < required {
            self.serve_pending = true;
            self.server = if live == 0 { self.last_conceder } else { 1 - self.last_conceder };
            self.serve_at = grid_time(m.bars, grid_after(m.bars, t + SERVE_GAP, DEFAULT_BAR), DEFAULT_BAR);
        }
        if self.serve_pending && t >= self.serve_at {
            self.serve_pending = false;
            if live >= required {
                return;
            }
            let Some(k) = self.orbs.iter().position(|o| o.state == OrbState::Dead) else { return };
            let s = self.server as usize & 1;
            let pos = zone_clamp(s, self.players[s].pos + vec2(fwd(s) * SERVE_DIST, 0.0));
            let bar = grid_time(m.bars, grid_after(m.bars, self.serve_at + BEAT_EPS, DEFAULT_BAR), DEFAULT_BAR);
            let id = self.new_id();
            self.orbs[k] = Orb { id, state: OrbState::Held, owner: s as u8, t0: self.serve_at, t1: bar.max(self.serve_at + 1.0), pos, land: pos.y, ..Orb::DEAD };
            self.push_fx(FxKind::Serve, s, pos);
        }
    }

    /// Point where player `i`'s live swing arc meets the orb's path `a -> b` this step.
    fn swing_hit(&self, i: usize, a: Vec2, b: Vec2) -> Option<Vec2> {
        let p = &self.players[i];
        let age = p.swing_age(self.t);
        if !(0.0..=SWING_ACTIVE).contains(&age) {
            return None;
        }
        let q = closest_on_seg(a, b, p.pos);
        let d = q - p.pos;
        let dist = d.length();
        if dist <= AVATAR_R + ORB_R {
            return Some(q);
        }
        if dist > SWING_R + ORB_R {
            return None;
        }
        if (d / dist).dot(p.aim) >= SWING_HALF.cos() {
            Some(q)
        } else {
            None
        }
    }

    fn knock(&mut self, i: usize, at: Vec2) {
        let p = &mut self.players[i];
        p.knock = vec2(-fwd(i), 0.0) * SUB_KNOCK;
        p.cd = p.cd.max(SUB_STUN);
        self.push_fx(FxKind::Slam, i, at);
    }

    fn step_orb(&mut self, k: usize, t: f32, m: &RallyMusic) {
        let mut o = self.orbs[k];
        match o.state {
            OrbState::Dead => {}
            OrbState::Held => {
                let s = o.owner as usize & 1;
                if let Some(q) = self.swing_hit(s, o.pos, o.pos) {
                    // a serve has no incoming strike beat: judged against the nearest beat
                    let aim = self.players[s].aim;
                    self.launch(&mut o, s, t, m, q, aim, Some(beat_off(m.beats, t)));
                } else if t >= o.t1 {
                    let from = o.pos;
                    self.launch(&mut o, s, t, m, from, vec2(fwd(s), 0.0), None);
                }
            }
            OrbState::Flying => {
                let prev = o.pos;
                o.pos = o.eval(t);
                let d = o.target();
                let body = closest_on_seg(prev, o.pos, self.players[d].pos).distance(self.players[d].pos) <= AVATAR_R + ORB_R;
                let hit = self.swing_hit(d, prev, o.pos);
                if o.power == Power::Sub && (body || hit.is_some()) {
                    o.power = Power::None;
                    self.knock(d, o.pos);
                }
                if let Some(q) = hit {
                    // timing is judged at contact: when the orb is at q vs its strike beat
                    let err = o.time_at_x(q.x) - o.tb;
                    if o.extra {
                        o.state = OrbState::Dead;
                        self.push_timed(FxKind::Hit, d, q, err_ms(err));
                    } else {
                        let aim = self.players[d].aim;
                        self.launch(&mut o, d, t, m, q, aim, Some(err));
                    }
                } else if t >= o.t1 {
                    let at = vec2(o.xe, o.land);
                    if o.land.abs() <= self.goal_half {
                        let a = o.owner as usize & 1;
                        self.scores[a] = self.scores[a].saturating_add(1);
                        self.last_conceder = d as u8;
                        o.state = OrbState::Dead;
                        self.push_fx(FxKind::Score, a, at);
                    } else if o.extra {
                        o.state = OrbState::Dead;
                        self.push_fx(FxKind::Bounce, d, at);
                    } else {
                        // off target: the end wall returns it for the defender (no power)
                        let v = o.eval(o.t1) - o.eval(o.t1 - 0.02);
                        let dir = vec2(fwd(d) * v.x.abs().max(1e-3), v.y);
                        self.launch(&mut o, d, t, m, at, dir, None);
                        // launch just pushed a Hit: show it as a bounce instead
                        let last = (self.fx_head as usize + MAX_FX - 1) % MAX_FX;
                        self.fx[last].kind = FxKind::Bounce;
                    }
                }
            }
        }
        self.orbs[k] = o;
    }

    /// Return orb `o` from point `from` by player `h` toward `aim`, beat-locked so it crosses the
    /// defender's strike line on a beat. `timing` = contact error vs the beat in s (None = untimed).
    #[allow(clippy::too_many_arguments)]
    fn launch(&mut self, o: &mut Orb, h: usize, t: f32, m: &RallyMusic, from: Vec2, aim: Vec2, timing: Option<f32>) {
        let perfect = timing.is_some_and(|e| e.abs() <= PERFECT_WIN);
        self.players[h].perfect = perfect;
        let err = timing.map_or(NO_ERR, err_ms);
        let f = fwd(h);
        let a = aim.y.atan2(aim.x * f);
        let a = if a.is_finite() { a.clamp(-RET_MAX_ANGLE, RET_MAX_ANGLE) } else { 0.0 };
        let d = vec2(f * a.cos(), a.sin());
        let xe = end_x(1 - h);
        let from = vec2(from.x.clamp(-HALF_W + 1.0, HALF_W - 1.0), fold(from.y));
        let tb = arrival_time(m, t, perfect);
        // constant x speed: strike line on `tb`, the end line a little after
        let frac = ((strike_x(1 - h) - from.x) / (xe - from.x)).clamp(0.05, 1.0);
        o.state = OrbState::Flying;
        o.owner = h as u8;
        o.p0 = from;
        o.t0 = t;
        o.tb = tb;
        o.t1 = t + (tb - t) / frac;
        o.xe = xe;
        o.y1 = from.y + (xe - from.x) / d.x * d.y;
        o.blen = beat_len_at(m.beats, tb - 1e-3);
        o.power = Power::None;
        o.curve = 0.0;
        o.land = fold(o.y1);
        o.pos = from;
        if !perfect {
            self.push_timed(FxKind::Hit, h, from, err);
            return;
        }
        self.push_timed(FxKind::Perfect, h, from, err);
        match self.players[h].ch {
            Character::Sub => {
                o.power = Power::Sub;
                self.push_fx(FxKind::Slam, h, from);
            }
            Character::Tick => o.power = Power::Tick,
            Character::Arc => {
                o.power = Power::Arc;
                o.curve = if h01(self.seed, o.id as u64 ^ 0xA7C) < 0.5 { -1.0 } else { 1.0 };
            }
            Character::Crack => {
                if let Some(k) = self.orbs.iter().position(|x| x.state == OrbState::Dead) {
                    let id = self.new_id();
                    let side = if o.y1 > 0.0 { -1.0 } else { 1.0 };
                    let mut e = Orb { id, extra: true, ..*o };
                    e.y1 = o.y1 + side * CRACK_SPREAD;
                    e.land = fold(e.y1);
                    o.y1 -= side * CRACK_SPREAD * 0.35;
                    o.land = fold(o.y1);
                    self.orbs[k] = e;
                    self.push_fx(FxKind::Split, h, from);
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Bot
    // -----------------------------------------------------------------------------------------

    /// A decent opponent: waits behind its strike line where the orb will cross, presses around
    /// the strike beat (`BOT_EARLY`..`BOT_LATE`), aims away from the other player, and misjudges
    /// about `BOT_MISS` of incoming orbs so games score.
    pub fn bot_input(&self, me: usize, t: f32, m: &RallyMusic) -> RallyInput {
        let me = me & 1;
        let p = &self.players[me];
        let opp = &self.players[1 - me];
        let f = fwd(me);
        let mut target = vec2(end_x(me) + f * ZONE_W * BOT_HOME_X, 0.0);
        // aim at the goal corner away from the opponent
        let side = if opp.pos.y > 0.0 { -1.0 } else { 1.0 };
        let goal_pt = vec2(end_x(1 - me), side * self.goal_half * BOT_AIM_GOAL);
        let mut aim = (goal_pt - p.pos).normalize_or_zero();
        let mut want = false;

        // most urgent orb: held for me, or flying at me and on target
        let mut best: Option<(f32, usize)> = None;
        for (k, o) in self.orbs.iter().enumerate() {
            let key = match o.state {
                OrbState::Held if o.owner as usize == me => o.t1,
                OrbState::Flying if o.target() == me && o.land.abs() <= self.goal_half + ORB_R => o.t1,
                _ => continue,
            };
            if best.map_or(true, |(b, _)| key < b) {
                best = Some((key, k));
            }
        }
        if let Some((_, k)) = best {
            let o = &self.orbs[k];
            if o.state == OrbState::Held {
                target = o.pos - vec2(f * BOT_STANDOFF, 0.0);
                let close = o.pos.distance(p.pos) < SWING_R * 0.8;
                want = close && (beat_dist(m.beats, t) < 0.02 || t > o.t1 - BOT_SERVE_WAIT);
            } else if t - o.t0 >= BOT_REACT {
                // wait just behind the strike line where the orb will cross it, press around its beat
                let pred = if o.tb > t { o.strike_point() } else { o.eval(t + 0.03) };
                let key = ((o.id as u64) << 8) ^ (o.tb.to_bits() as u64) << 1 ^ me as u64;
                let miss = h01(self.seed, key);
                let err = if miss < BOT_MISS { if miss < BOT_MISS * 0.5 { BOT_MISS_OFF } else { -BOT_MISS_OFF } } else { 0.0 };
                target = vec2(strike_x(me) - f * BOT_STANDOFF, pred.y + err);
                // face between the incoming orb and the goal so the arc covers it
                let to_orb = (o.pos - p.pos).normalize_or_zero();
                aim = (aim + to_orb).normalize_or_zero();
                let jit = -BOT_EARLY + h01(self.seed, key ^ 0x5EED) * (BOT_EARLY + BOT_LATE);
                want = t >= o.tb + jit && t < o.tb + BOT_LATE + SWING_ACTIVE;
            }
        }
        let mv = ((target - p.pos) / (PLAYER_SPEED * BOT_GAIN)).clamp_length_max(1.0);
        RallyInput { mv, aim: if aim == Vec2::ZERO { vec2(f, 0.0) } else { aim }, swing: want && p.cd <= 0.0 && !p.prev }
    }

    // -----------------------------------------------------------------------------------------
    // Snapshots
    // -----------------------------------------------------------------------------------------

    /// Host snapshot (about 400 bytes at most). Appends to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let f = |out: &mut Vec<u8>, v: f32| out.extend_from_slice(&v.to_le_bytes());
        out.push(SNAP_VER);
        f(out, self.t);
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.extend_from_slice(&self.next_id.to_le_bytes());
        out.extend_from_slice(&self.scores[0].to_le_bytes());
        out.extend_from_slice(&self.scores[1].to_le_bytes());
        f(out, self.goal_half);
        out.push(self.serve_pending as u8 | (self.server & 1) << 1 | (self.last_conceder & 1) << 2);
        f(out, self.serve_at);
        out.push(section_index(self.section));
        for p in &self.players {
            for v in [p.pos.x, p.pos.y, p.aim.x, p.aim.y, p.knock.x, p.knock.y, p.swing_t, p.cd] {
                f(out, v);
            }
            out.push(p.ch.index());
            out.push(p.perfect as u8 | (p.prev as u8) << 1);
        }
        let n = self.orbs.iter().filter(|o| o.state != OrbState::Dead).count();
        out.push(n as u8);
        for o in self.orbs.iter().filter(|o| o.state != OrbState::Dead) {
            out.extend_from_slice(&o.id.to_le_bytes());
            out.push(o.state as u8);
            out.push(o.extra as u8 | (o.owner & 1) << 1);
            out.push(o.power as u8);
            for v in [o.p0.x, o.p0.y, o.t0, o.t1, o.tb, o.xe, o.y1, o.curve, o.blen, o.land, o.pos.x, o.pos.y] {
                f(out, v);
            }
        }
        out.push(self.events().count() as u8);
        // oldest first, so the client's ring overwrites in the same order as the host's
        let head = self.fx_head as usize;
        for e in (0..MAX_FX).map(|j| &self.fx[(head + j) % MAX_FX]).filter(|e| e.t > NEVER * 0.5) {
            out.push(e.kind as u8);
            out.push(e.who & 1);
            for v in [e.pos.x, e.pos.y, e.t] {
                f(out, v);
            }
            out.extend_from_slice(&e.err.to_le_bytes());
        }
    }

    /// Client: replace the state from a snapshot. Returns false (state untouched) if malformed.
    pub fn decode(&mut self, b: &[u8]) -> bool {
        match self.parse(b) {
            Some(r) => {
                *self = r;
                true
            }
            None => false,
        }
    }

    fn parse(&self, b: &[u8]) -> Option<Rally> {
        let mut rd = Rd { b, i: 0 };
        if rd.u8()? != SNAP_VER {
            return None;
        }
        let mut r = self.clone();
        r.t = rd.f32()?;
        r.seed = rd.u64()?;
        r.next_id = rd.u16()?;
        r.scores = [rd.u32()?, rd.u32()?];
        r.goal_half = rd.f32()?;
        let fl = rd.u8()?;
        if fl > 7 {
            return None;
        }
        r.serve_pending = fl & 1 != 0;
        r.server = fl >> 1 & 1;
        r.last_conceder = fl >> 2 & 1;
        r.serve_at = rd.f32()?;
        r.section = match rd.u8()? {
            0 => SectionKind::Calm,
            1 => SectionKind::Normal,
            2 => SectionKind::Intense,
            3 => SectionKind::Drop,
            _ => return None,
        };
        for p in r.players.iter_mut() {
            let mut v = [0.0f32; 8];
            for x in v.iter_mut() {
                *x = rd.f32()?;
            }
            p.pos = vec2(v[0], v[1]);
            p.aim = vec2(v[2], v[3]);
            p.knock = vec2(v[4], v[5]);
            p.swing_t = v[6];
            p.cd = v[7];
            p.ch = Character::from_index(rd.u8()?)?;
            let pf = rd.u8()?;
            if pf > 3 {
                return None;
            }
            p.perfect = pf & 1 != 0;
            p.prev = pf & 2 != 0;
        }
        let n = rd.u8()? as usize;
        if n > MAX_ORBS {
            return None;
        }
        r.orbs = [Orb::DEAD; MAX_ORBS];
        for o in r.orbs.iter_mut().take(n) {
            o.id = rd.u16()?;
            o.state = match rd.u8()? {
                1 => OrbState::Held,
                2 => OrbState::Flying,
                _ => return None,
            };
            let bits = rd.u8()?;
            if bits > 3 {
                return None;
            }
            o.extra = bits & 1 != 0;
            o.owner = bits >> 1 & 1;
            o.power = match rd.u8()? {
                0 => Power::None,
                1 => Power::Sub,
                2 => Power::Tick,
                3 => Power::Arc,
                _ => return None,
            };
            let mut v = [0.0f32; 12];
            for x in v.iter_mut() {
                *x = rd.f32()?;
            }
            o.p0 = vec2(v[0], v[1]);
            o.t0 = v[2];
            o.t1 = v[3];
            o.tb = v[4];
            o.xe = v[5];
            o.y1 = v[6];
            o.curve = v[7];
            o.blen = v[8];
            o.land = v[9];
            o.pos = vec2(v[10], v[11]);
        }
        let n = rd.u8()? as usize;
        if n > MAX_FX {
            return None;
        }
        for e in r.fx.iter_mut() {
            e.t = NEVER;
        }
        for e in r.fx.iter_mut().take(n) {
            e.kind = *FX_KINDS.get(rd.u8()? as usize)?;
            let who = rd.u8()?;
            if who > 1 {
                return None;
            }
            e.who = who;
            e.pos = vec2(rd.f32()?, rd.f32()?);
            e.t = rd.f32()?;
            e.err = i16::from_le_bytes(rd.take()?);
        }
        r.fx_head = (n % MAX_FX) as u8;
        if rd.i != b.len() {
            return None;
        }
        Some(r)
    }

    /// Client smoothing: `self` becomes snapshot `b` with positions blended from `a` by `k`
    /// (0 = a, 1 = b). Orbs are matched by id; new or relaunched orbs snap.
    pub fn interpolate(&mut self, a: &Rally, b: &Rally, k: f32) {
        let k = if k.is_finite() { k.clamp(0.0, 1.0) } else { 1.0 };
        self.clone_from(b);
        self.t = a.t + (b.t - a.t) * k;
        for i in 0..2 {
            let (pa, pb) = (&a.players[i], &b.players[i]);
            self.players[i].pos = pa.pos.lerp(pb.pos, k);
            self.players[i].aim = pa.aim.lerp(pb.aim, k).try_normalize().unwrap_or(pb.aim);
        }
        for o in self.orbs.iter_mut().filter(|o| o.state != OrbState::Dead) {
            if let Some(oa) = a.orbs.iter().find(|x| x.state == o.state && x.id == o.id && x.t0 == o.t0) {
                o.pos = if o.state == OrbState::Flying { o.eval(self.t) } else { oa.pos.lerp(o.pos, k) };
            }
        }
    }
}

/// Bounds-checked little-endian reader; every float must be finite and sane.
struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl Rd<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let s = self.b.get(self.i..self.i.checked_add(N)?)?;
        self.i += N;
        s.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take()?))
    }
    fn f32(&mut self) -> Option<f32> {
        let v = f32::from_le_bytes(self.take()?);
        (v.is_finite() && v.abs() < 1.0e7).then_some(v)
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const BPM_BEAT: f32 = 0.5;

    fn grid(step: f32, n: usize, jitter: bool) -> Vec<f32> {
        (0..n).map(|i| i as f32 * step + if jitter { 0.004 * ((i * 7 % 5) as f32 - 2.0) } else { 0.0 }).collect()
    }

    fn music<'a>(beats: &'a [f32], bars: &'a [f32], section: SectionKind) -> RallyMusic<'a> {
        RallyMusic { beats, bars, section, next_build: 0.0 }
    }

    /// Steps idle until the serve appears for player `s`, returns the time.
    fn until_held(r: &mut Rally, m: &RallyMusic, mut t: f32, dt: f32) -> f32 {
        for _ in 0..20_000 {
            t += dt;
            r.step(dt, t, [RallyInput::default(); 2], m);
            if r.orbs.iter().any(|o| o.state == OrbState::Held) {
                return t;
            }
        }
        panic!("no serve");
    }

    fn swing(r: &mut Rally, m: &RallyMusic, who: usize, t: f32, dt: f32, aim: Vec2) {
        let mut inp = [RallyInput::default(); 2];
        inp[who] = RallyInput { mv: Vec2::ZERO, aim, swing: true };
        r.step(dt, t, inp, m);
    }

    #[test]
    fn orb_arrives_on_a_beat() {
        let beats = grid(BPM_BEAT, 200, true);
        let bars = grid(BPM_BEAT * 4.0, 50, false);
        for sec in [SectionKind::Calm, SectionKind::Normal, SectionKind::Intense, SectionKind::Drop] {
            let m = music(&beats, &bars, sec);
            let mut r = Rally::new(7, [Character::Sub, Character::Tick]);
            r.last_conceder = 0;
            let dt = 0.001;
            let mut t = until_held(&mut r, &m, 0.0, dt);
            t += 0.123; // off-beat swing
            swing(&mut r, &m, 0, t, dt, vec2(1.0, 0.15));
            let o = r.orbs.iter().find(|o| o.state == OrbState::Flying).copied().expect("launched");
            let mut crossed = None;
            for _ in 0..10_000 {
                t += dt;
                r.step(dt, t, [RallyInput { mv: vec2(0.0, -1.0), ..Default::default() }; 2], &m);
                let x = r.orbs.iter().find(|x| x.id == o.id).map(|x| x.pos.x);
                if x.map_or(true, |x| x >= strike_x(1)) {
                    crossed = Some(t);
                    break;
                }
            }
            let tc = crossed.expect("orb reached the strike line");
            assert!(beat_dist(&beats, tc) <= 0.005, "{sec:?}: crossed at {tc}, {} from a beat", beat_dist(&beats, tc));
            assert!(o.tb - o.t0 >= min_flight(sec) * BPM_BEAT - 0.03);
            assert!(o.t1 > o.tb, "the end line comes after the strike line");
        }
    }

    /// Serves an off-beat straight orb from player 0 to player 1; returns (rally, t, orb).
    fn incoming(m: &RallyMusic, dt: f32) -> (Rally, f32, Orb) {
        let mut r = Rally::new(21, [Character::Crack, Character::Tick]);
        r.last_conceder = 0;
        let mut t = until_held(&mut r, m, 0.0, dt) + 0.2;
        swing(&mut r, m, 0, t, dt, vec2(1.0, 0.0));
        let o = *r.orbs.iter().find(|o| o.state == OrbState::Flying).expect("launched");
        t += dt;
        (r, t, o)
    }

    /// Player 1 stands at `pos` facing the orb, presses at `press`; returns the contact Fx.
    fn contact(m: &RallyMusic, pos_of: impl Fn(&Orb) -> Vec2, press_before_tb: f32) -> Fx {
        let dt = 1.0 / 240.0;
        let (mut r, mut t, o) = incoming(m, dt);
        r.players[1].pos = pos_of(&o);
        let mut pressed = false;
        while t < o.t1 + 0.2 {
            let mut inp = [RallyInput::default(); 2];
            inp[1].aim = vec2(-1.0, 0.0);
            if !pressed && t >= o.tb - press_before_tb {
                inp[1].swing = true;
                pressed = true;
            }
            r.step(dt, t, inp, m);
            if let Some(e) = r.events().find(|e| e.who == 1 && matches!(e.kind, FxKind::Hit | FxKind::Perfect)) {
                return *e;
            }
            t += dt;
        }
        panic!("no contact");
    }

    #[test]
    fn perfect_is_judged_at_the_strike_line() {
        let beats = grid(BPM_BEAT, 200, false);
        let bars = grid(BPM_BEAT * 4.0, 50, false);
        let m = music(&beats, &bars, SectionKind::Normal);
        // on the line: just behind the strike point, pressing as the orb arrives
        let e = contact(&m, |o| o.strike_point() + vec2(40.0, 0.0), 0.02);
        assert_eq!(e.kind, FxKind::Perfect, "err {}ms", e.err);
        assert!((e.err as f32).abs() <= PERFECT_WIN * 1000.0);
        // 150 ms early: standing where the orb is 150 ms before the beat
        let e = contact(&m, |o| o.eval(o.tb - 0.15) + vec2(30.0, 0.0), 0.17);
        assert_eq!(e.kind, FxKind::Hit, "err {}ms", e.err);
        assert!(e.err < -(PERFECT_WIN * 1000.0) as i16, "early err {}ms", e.err);
    }

    #[test]
    fn perfect_return_one_beat_sooner() {
        let beats = grid(BPM_BEAT, 200, false);
        let bars = grid(BPM_BEAT * 4.0, 50, false);
        let m = music(&beats, &bars, SectionKind::Normal);
        for k in 10..20 {
            let bt = beats[k];
            for off in [-0.06, -0.02, 0.0, 0.03, 0.065] {
                let t = bt + off;
                let n = arrival_time(&m, t, false);
                let p = arrival_time(&m, t, true);
                assert!((n - p - BPM_BEAT).abs() < 1e-4, "t={t}: normal {n} perfect {p}");
            }
        }
        // through the sim: a swing exactly on a beat is perfect and lands one beat early
        let mut r = Rally::new(3, [Character::Crack, Character::Arc]);
        r.last_conceder = 0;
        let dt = 1.0 / 120.0;
        let t = until_held(&mut r, &m, 0.0, dt);
        let k = grid_after(&beats, t + 0.05, DEFAULT_BEAT) as usize;
        let mut tt = t;
        while tt + dt < beats[k] {
            tt += dt;
            r.step(dt, tt, [RallyInput::default(); 2], &m);
        }
        swing(&mut r, &m, 0, beats[k], beats[k] - tt, vec2(1.0, 0.0));
        assert!(r.players[0].perfect);
        let o = r.orbs.iter().find(|o| o.state == OrbState::Flying && !o.extra).expect("launched");
        assert!((o.tb - beats[k + 1]).abs() < 1e-4, "perfect tb {} vs {}", o.tb, beats[k + 1]);
        assert_eq!(r.orbs.iter().filter(|o| o.extra).count(), 1, "crack splits");
    }

    #[test]
    fn scoring_and_serve() {
        let beats = grid(BPM_BEAT, 200, false);
        let bars = grid(BPM_BEAT * 4.0, 50, false);
        let m = music(&beats, &bars, SectionKind::Normal);
        let mut r = Rally::new(11, [Character::Sub, Character::Sub]);
        r.last_conceder = 0;
        let dt = 1.0 / 60.0;
        let mut t = until_held(&mut r, &m, 0.0, dt);
        let held = r.orbs.iter().find(|o| o.state == OrbState::Held).unwrap();
        assert_eq!(held.owner, 0);
        assert!(held.pos.x < 0.0);
        assert!(bars.iter().any(|b| (b - held.t0).abs() < 1e-4), "serve on a downbeat");
        t += dt;
        swing(&mut r, &m, 0, t, dt, vec2(1.0, 0.0));
        // player 1 runs to the top corner and never swings
        let away = [RallyInput::default(), RallyInput { mv: vec2(1.0, -1.0), ..Default::default() }];
        for _ in 0..600 {
            t += dt;
            r.step(dt, t, away, &m);
            if r.score() != [0, 0] {
                break;
            }
        }
        assert_eq!(r.score(), [1, 0]);
        assert_eq!(r.last_conceder, 1);
        assert!(r.events().any(|e| e.kind == FxKind::Score && e.who == 0));
        let t2 = until_held(&mut r, &m, t, dt);
        let held = r.orbs.iter().find(|o| o.state == OrbState::Held).unwrap();
        assert_eq!(held.owner, 1, "the conceder serves");
        assert!(held.pos.x > 0.0);
        assert!(bars.iter().any(|b| (b - held.t0).abs() < 1e-4));
        assert!(held.t0 >= t + SERVE_GAP - 1e-3 && held.t0 <= t2 + 1e-3);
    }

    #[test]
    fn snapshot_round_trip() {
        let beats = grid(BPM_BEAT, 200, false);
        let bars = grid(BPM_BEAT * 4.0, 50, false);
        let m = music(&beats, &bars, SectionKind::Drop);
        let mut r = Rally::new(99, [Character::Tick, Character::Crack]);
        let dt = 1.0 / 60.0;
        let mut t = 0.0;
        let mut buf = Vec::new();
        for _ in 0..1200 {
            t += dt;
            let inp = [r.bot_input(0, t, &m), r.bot_input(1, t, &m)];
            r.step(dt, t, inp, &m);
        }
        buf.clear();
        r.encode(&mut buf);
        assert!(buf.len() < 512, "snapshot {} bytes", buf.len());
        let mut c = Rally::new(0, [Character::Sub, Character::Sub]);
        assert!(c.decode(&buf));
        let mut buf2 = Vec::new();
        c.encode(&mut buf2);
        assert_eq!(buf, buf2);
        // the decoded copy continues identically
        for _ in 0..300 {
            t += dt;
            let inp = [r.bot_input(0, t, &m), r.bot_input(1, t, &m)];
            r.step(dt, t, inp, &m);
            let inp = [c.bot_input(0, t, &m), c.bot_input(1, t, &m)];
            c.step(dt, t, inp, &m);
        }
        buf.clear();
        buf2.clear();
        r.encode(&mut buf);
        c.encode(&mut buf2);
        assert_eq!(buf, buf2);
        // worst case size: every orb and fx slot full
        let mut w = r.clone();
        for o in w.orbs.iter_mut() {
            o.state = OrbState::Flying;
        }
        for e in w.fx.iter_mut() {
            e.t = 1.0;
        }
        buf.clear();
        w.encode(&mut buf);
        assert!(buf.len() < 512, "worst-case snapshot {} bytes", buf.len());
        // interpolate stays sane
        let mut i = r.clone();
        i.interpolate(&c, &r, 0.5);
        assert!(i.players.iter().all(|p| finite(p.pos)));
    }

    #[test]
    fn malformed_snapshot_rejected() {
        let r = Rally::new(5, [Character::Arc, Character::Sub]);
        let mut good = Vec::new();
        r.encode(&mut good);
        let mut c = r.clone();
        assert!(c.decode(&good));
        assert!(!c.decode(&[]));
        for n in 0..good.len() {
            assert!(!c.decode(&good[..n]), "truncated at {n}");
        }
        let mut long = good.clone();
        long.push(0);
        assert!(!c.decode(&long), "trailing bytes");
        let mut bad = good.clone();
        bad[0] ^= 0xFF;
        assert!(!c.decode(&bad), "version");
        let mut nan = good.clone();
        nan[1..5].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(!c.decode(&nan), "nan");
        // character index out of range (player 0's char byte: after header + 8 floats)
        let ch = 1 + 4 + 8 + 2 + 8 + 4 + 1 + 4 + 1 + 32;
        let mut badc = good.clone();
        badc[ch] = 9;
        assert!(!c.decode(&badc), "char");
        // orb count too large
        let n_at = ch + 2 + 34;
        let mut bado = good.clone();
        bado[n_at] = 200;
        assert!(!c.decode(&bado), "orb count");
        // random garbage never panics
        let mut rng = Rng::new(1);
        for _ in 0..2000 {
            let len = rng.below(500);
            let mut g: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
            if !g.is_empty() && rng.chance(0.5) {
                g[0] = SNAP_VER;
            }
            let _ = c.decode(&g);
        }
    }

    #[test]
    fn bots_trade_points_for_a_minute() {
        let beats = grid(BPM_BEAT, 200, true);
        let bars = grid(BPM_BEAT * 4.0, 50, false);
        let mut totals = [0u32; 2];
        for seed in [1u64, 2, 3] {
            let chars = [Character::ALL[seed as usize % 4], Character::ALL[(seed as usize + 1) % 4]];
            let mut r = Rally::new(seed, chars);
            let dt = 1.0 / 60.0;
            let mut t = 0.0f32;
            let mut perfects = 0;
            let mut seen = [NEVER; MAX_FX];
            while t < 60.0 {
                t += dt;
                let sec = match (t / 15.0) as u32 {
                    0 => SectionKind::Calm,
                    1 => SectionKind::Normal,
                    2 => SectionKind::Intense,
                    _ => SectionKind::Drop,
                };
                let mut m = music(&beats, &bars, sec);
                m.next_build = ((t - 40.0) / 5.0).clamp(0.0, 1.0) * if t < 45.0 { 1.0 } else { 0.0 };
                let inp = [r.bot_input(0, t, &m), r.bot_input(1, t, &m)];
                r.step(dt, t, inp, &m);
                for (i, e) in r.fx.iter().enumerate() {
                    if e.t != seen[i] {
                        seen[i] = e.t;
                        perfects += (e.kind == FxKind::Perfect) as u32;
                    }
                }
                for o in r.orbs.iter().filter(|o| o.state != OrbState::Dead) {
                    assert!(finite(o.pos) && o.pos.x.abs() <= HALF_W + 1.0 && o.pos.y.abs() <= HALF_H);
                }
            }
            let s = r.score();
            eprintln!("seed {seed} {chars:?}: score {s:?}, perfects {perfects}");
            assert!(s[0] + s[1] >= 3, "seed {seed}: too few points {s:?}");
            assert!(s[0] + s[1] <= 60, "seed {seed}: rallies never last {s:?}");
            totals[0] += s[0];
            totals[1] += s[1];
        }
        assert!(totals[0] > 0 && totals[1] > 0, "both bots score: {totals:?}");
    }
}
