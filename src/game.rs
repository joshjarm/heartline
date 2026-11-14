//! The simulation: player, the Heart, bullets, enemies, tiles, lasers, shockwaves, pickups,
//! upgrades and scoring. No rendering here.

use macroquad::math::{vec2, Vec2};
use std::f32::consts::TAU;

use crate::analysis::{Feat, SectionKind};
use crate::engine::Sfx;
use crate::level::*;
use crate::rng::Rng;

pub const HEART_R: f32 = 62.0;
pub const PLAYER_R: f32 = 11.0;
const PLAYER_HIT_R: f32 = 5.0;
const MAX_BULLETS: usize = 1500;
const MAX_ENEMIES: usize = 30;
const MAX_PARTS: usize = 2600;

// Palette slots used by particles / floating text.
pub const C_PRIMARY: u8 = 0;
pub const C_SECONDARY: u8 = 1;
pub const C_ACCENT: u8 = 2;
pub const C_DANGER: u8 = 3;
pub const C_WHITE: u8 = 4;

#[derive(Clone, Copy, Default)]
pub struct Input {
    pub mv: Vec2,
    pub aim: Vec2,
    pub fire: bool,
    pub dash: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeId {
    Tempo,
    Chord,
    Gain,
    Pierce,
    Sustain,
    Glide,
    Phase,
    Magnet,
    Echo,
    Encore,
}

impl UpgradeId {
    pub const ALL: [UpgradeId; 10] = [
        UpgradeId::Tempo,
        UpgradeId::Chord,
        UpgradeId::Gain,
        UpgradeId::Pierce,
        UpgradeId::Sustain,
        UpgradeId::Glide,
        UpgradeId::Phase,
        UpgradeId::Magnet,
        UpgradeId::Echo,
        UpgradeId::Encore,
    ];
    pub fn name(self) -> &'static str {
        match self {
            UpgradeId::Tempo => "TEMPO",
            UpgradeId::Chord => "CHORD",
            UpgradeId::Gain => "GAIN",
            UpgradeId::Pierce => "PIERCE",
            UpgradeId::Sustain => "SUSTAIN",
            UpgradeId::Glide => "GLIDE",
            UpgradeId::Phase => "PHASE",
            UpgradeId::Magnet => "MAGNET",
            UpgradeId::Echo => "ECHO",
            UpgradeId::Encore => "ENCORE",
        }
    }
    pub fn desc(self) -> &'static str {
        match self {
            UpgradeId::Tempo => "fire on faster beat divisions",
            UpgradeId::Chord => "+1 shot per volley",
            UpgradeId::Gain => "+50% shot damage",
            UpgradeId::Pierce => "shots pierce +1",
            UpgradeId::Sustain => "+1 max HP, heal 2",
            UpgradeId::Glide => "+12% move speed",
            UpgradeId::Phase => "dash recharges faster",
            UpgradeId::Magnet => "bigger pickup range",
            UpgradeId::Echo => "drone that fires on the beat",
            UpgradeId::Encore => "shield, recharges each section",
        }
    }
    pub fn max(self) -> u8 {
        match self {
            UpgradeId::Tempo => 4,
            UpgradeId::Chord => 4,
            UpgradeId::Gain => 6,
            UpgradeId::Pierce => 3,
            UpgradeId::Sustain => 5,
            UpgradeId::Glide => 4,
            UpgradeId::Phase => 3,
            UpgradeId::Magnet => 3,
            UpgradeId::Echo => 3,
            UpgradeId::Encore => 1,
        }
    }
}

/// Beat subdivisions the gun may lock to, and the target seconds between shots per TEMPO level.
const FIRE_DIVS: [f32; 9] = [0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0];
const FIRE_INTERVALS: [f32; 5] = [0.27, 0.21, 0.165, 0.13, 0.105];

// ---- difficulty ramp ("pressure") -----------------------------------------------------------
// The level generator plans every song at full density for the chosen difficulty. At run time
// the World scales that plan by `pressure()`: a ramp that eases the player into each song,
// grows a little with every cleared track, and backs off for a few seconds after a hit.

/// (starting pressure, seconds of play until full pressure) for the 1st, 2nd and later songs.
const RAMP: [(f32, f32); 3] = [(0.40, 75.0), (0.70, 30.0), (0.85, 15.0)];
/// Extra pressure per cleared song in a run.
const RUN_ESCALATION: f32 = 0.07;
/// Pressure multiplier right after the player is hit, and how fast it recovers (per second).
const MERCY_DROP: f32 = 0.55;
const MERCY_RECOVER: f32 = 0.05;
/// Mechanics switch on as pressure passes these thresholds (so a run introduces them in order).
const P_SPAWN: f32 = 0.44;
const P_TILES: f32 = 0.55;
const P_WAVE: f32 = 0.60;
const P_LASER: f32 = 0.66;
const P_ELITE: f32 = 0.75;
const P_TICK: f32 = 0.80;
/// Seconds of invulnerability after a hit, and the radius in which bullets are cleared.
const HIT_INVULN: f32 = 1.6;
const HIT_CLEAR_R: f32 = 220.0;

// Hit sources (index into `Stats::by_src`).
pub const SRC_BULLET: usize = 0;
pub const SRC_ENEMY: usize = 1;
pub const SRC_TILE: usize = 2;
pub const SRC_LASER: usize = 3;
pub const SRC_WAVE: usize = 4;
pub const SRC_HEART: usize = 5;

fn tile_tag(p: TilePattern) -> u8 {
    match p {
        TilePattern::Checker(..) => 0,
        TilePattern::Rows(..) => 1,
        TilePattern::Cols(..) => 2,
        TilePattern::Ring(..) => 3,
        TilePattern::Cross(..) => 4,
        TilePattern::Diag(..) => 5,
        TilePattern::Scatter(..) => 6,
        TilePattern::ColBand(..) => 7,
        TilePattern::RowBand(..) => 8,
    }
}

fn smooth01(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Decay rate (per second) of the `World::hit` drum envelopes.
const HIT_DECAY: f32 = 5.5;
/// Groove envelope: speed = 1 + amt * (PEAK * exp(-DECAY * beat_phase) - MEAN). MEAN is the
/// average of the exponential term over one beat, so the average speed stays exactly 1.
const GROOVE_PEAK: f32 = 2.0;
const GROOVE_DECAY: f32 = 5.0;
const GROOVE_MEAN: f32 = GROOVE_PEAK * 0.198_652; // PEAK * (1 - e^-5) / 5

pub struct Player {
    pub pos: Vec2,
    pub vel: Vec2,
    pub aim_dir: Vec2,
    pub hp: i32,
    pub max_hp: i32,
    pub invuln: f32,
    pub dash_t: f32,
    pub dash_cd: f32,
    pub dash_cd_max: f32,
    pub dash_dir: Vec2,
    pub shield: bool,
    pub ups: [u8; 10],
    pub xp: u32,
    pub xp_next: u32,
    pub level: u32,
    fire_phase: i64,
    pub drone_ang: f32,
    pub muzzle: f32,
}

impl Player {
    fn new() -> Self {
        Player {
            pos: vec2(0.0, 250.0),
            vel: Vec2::ZERO,
            aim_dir: vec2(0.0, -1.0),
            hp: 6,
            max_hp: 6,
            invuln: 0.0,
            dash_t: 0.0,
            dash_cd: 0.0,
            dash_cd_max: 1.1,
            dash_dir: vec2(0.0, -1.0),
            shield: false,
            ups: [0; 10],
            xp: 0,
            xp_next: 14,
            level: 0,
            fire_phase: 0,
            drone_ang: 0.0,
            muzzle: 0.0,
        }
    }
    pub fn up(&self, id: UpgradeId) -> u8 {
        self.ups[id as usize]
    }
    pub fn magnet_r(&self) -> f32 {
        150.0 * (1.0 + 0.6 * self.up(UpgradeId::Magnet) as f32)
    }
    pub fn drones(&self) -> usize {
        self.up(UpgradeId::Echo) as usize
    }
    pub fn drone_pos(&self, k: usize) -> Vec2 {
        let n = self.drones().max(1) as f32;
        let a = self.drone_ang + k as f32 * TAU / n;
        self.pos + vec2(a.cos(), a.sin()) * 44.0
    }
}

/// Hot round bullet: must be dodged.
pub const B_ORB: u8 = 0;
/// White square "note": can be shot down (`soft == true`).
pub const B_NOTE: u8 = 1;
/// Small fast dart fired on hi-hats: must be dodged.
pub const B_TICK: u8 = 2;

pub struct Bullet {
    pub pos: Vec2,
    pub vel: Vec2,
    pub r: f32,
    pub soft: bool,
    pub age: f32,
    pub fade: f32,
    /// One of `B_ORB`, `B_NOTE`, `B_TICK` (visual identity; `soft` decides whether it can be shot).
    pub kind: u8,
    /// What fired it (pattern index, 9 = hi-hat dart, 10 = orbiter, 11 = elite); for balance stats.
    pub pat: u8,
}

/// A Heart pulse that has been announced but has not fired yet. It fires at `t`; the announcement
/// started at `t - lead`. Draw it as an approach ring closing on the Heart.
pub struct PendingPulse {
    pub t: f32,
    pub lead: f32,
    pub pattern: Pattern,
    pub count: u16,
    pub speed: f32,
    pub rot: f32,
    pub soft: bool,
    /// Angle (radians) from the Heart to the player, locked when the pulse was announced.
    /// Aimed patterns (`Pattern::aimed`) are laid out around it.
    pub aim: f32,
}

pub struct Shot {
    pub pos: Vec2,
    pub vel: Vec2,
    pub dmg: f32,
    pub pierce: u8,
    pub life: f32,
    pub r: f32,
}

pub struct Enemy {
    pub kind: EnemyKind,
    pub pos: Vec2,
    pub vel: Vec2,
    pub hp: f32,
    pub max_hp: f32,
    pub r: f32,
    pub rot: f32,
    pub age: f32,
    pub flash: f32,
    pub state: f32,
}

pub struct Portal {
    pub kind: EnemyKind,
    pub pos: Vec2,
    pub t0: f32,
    pub t1: f32,
}

pub struct Laser {
    pub horizontal: bool,
    pub from: f32,
    pub to: f32,
    pub t0: f32,
    pub dur: f32,
    pub lead: f32,
}

impl Laser {
    pub fn pos_at(&self, t: f32) -> f32 {
        let k = ((t - self.t0) / self.dur.max(0.01)).clamp(0.0, 1.0);
        self.from + (self.to - self.from) * k
    }
    pub fn active(&self, t: f32) -> bool {
        t >= self.t0 && t <= self.t0 + self.dur
    }
}

pub struct Wave {
    pub t0: f32,
    pub lead: f32,
    pub gap: f32,
    pub gaps: u8,
    pub speed: f32,
}

impl Wave {
    pub fn radius(&self, t: f32) -> f32 {
        HEART_R + (t - self.t0).max(0.0) * self.speed
    }
    pub fn in_gap(&self, ang: f32) -> bool {
        let n = self.gaps.max(1) as f32;
        let half = 0.36;
        for k in 0..self.gaps.max(1) {
            let c = self.gap + k as f32 * TAU / n;
            let mut d = (ang - c).rem_euclid(TAU);
            if d > std::f32::consts::PI {
                d -= TAU;
            }
            if d.abs() < half {
                return true;
            }
        }
        false
    }
}

pub struct Shard {
    pub pos: Vec2,
    pub vel: Vec2,
    pub life: f32,
    pub value: u8,
    pub heal: bool,
    pub age: f32,
}

pub struct Orb {
    pub pos: Vec2,
    pub up: UpgradeId,
    pub life: f32,
    pub age: f32,
}

pub struct Particle {
    pub pos: Vec2,
    pub vel: Vec2,
    pub life: f32,
    pub max: f32,
    pub size: f32,
    pub col: u8,
    pub rot: f32,
}

pub struct FloatText {
    pub pos: Vec2,
    pub text: String,
    pub life: f32,
    pub max: f32,
    pub col: u8,
    pub size: f32,
}

#[derive(Clone, Copy)]
pub struct Tile {
    pub strike_t: f32,
    pub lead: f32,
    pub armed: bool,
    pub flash: f32,
    /// Which kind of tile pattern armed it (balance stats only).
    pub tag: u8,
}

pub struct Heart {
    pub hp: f32,
    pub max_hp: f32,
    pub broken: bool,
    pub punch: f32,
    pub flash: f32,
    pub rot: f32,
    pub reform: f32,
    pub charge: f32,
}

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub kills: u32,
    pub shards: u32,
    pub hits: u32,
    pub max_combo: f32,
    pub hearts_broken: u32,
    pub songs: u32,
    pub notes_shot: u32,
    /// Hits taken per source (`SRC_*`).
    pub by_src: [u32; 6],
    /// Bullet hits per `Bullet::pat`.
    pub by_pat: [u32; 12],
    /// Tile hits per tile-pattern kind (see `tile_tag`).
    pub by_tile: [u32; 9],
}

pub struct World {
    pub t: f32,
    pub player: Player,
    pub heart: Heart,
    pub bullets: Vec<Bullet>,
    pub shots: Vec<Shot>,
    pub enemies: Vec<Enemy>,
    pub portals: Vec<Portal>,
    pub lasers: Vec<Laser>,
    pub waves: Vec<Wave>,
    pub shards: Vec<Shard>,
    pub orbs: Vec<Orb>,
    pub parts: Vec<Particle>,
    pub texts: Vec<FloatText>,
    pub tiles: Vec<Tile>,
    /// Announced Heart pulses that have not fired yet (sorted by nothing in particular).
    pub pulses: Vec<PendingPulse>,
    /// Decaying 0..1 envelopes of the most recent kick / snare / hat onset (index = band).
    pub hit: [f32; 3],
    /// Position inside the current beat, 0..1 (0 = on the beat).
    pub beat_phase: f32,
    /// Seconds per beat of the current song (0.5 when unknown).
    pub beat_len: f32,
    /// Current speed multiplier applied to bullets and enemies: they surge on every beat and
    /// ease off between beats (averages 1.0 over a beat).
    pub groove: f32,
    /// How strongly `groove` swings, 0..1 (set per section).
    pub groove_amt: f32,
    /// 0..1 transition envelope: kicked to 1 when a song / menu track starts, decays over ~1 s.
    pub warp: f32,
    /// 0..1 difficulty ramp of the current moment (for HUD / visuals).
    pub threat: f32,
    pub shake: f32,
    pub flash: f32,
    pub beat_pulse: f32,
    pub bar_pulse: f32,
    pub beat_count: u32,
    pub score: u64,
    pub combo: f32,
    combo_idle: f32,
    pub stats: Stats,
    pub section_kind: SectionKind,
    pub section_idx: u16,
    pub section_banner: f32,
    pub palette: u8,
    pub intensity: f32,
    pub feat: Feat,
    pub rng: Rng,
    pub no_fail: bool,
    pub diff: f32,
    pub ended: bool,
    pub dead: bool,
    pub song_idx: u32,
    pending_orbs: u32,
    /// Seconds of actual play (music audible) in the current song.
    pub active_t: f32,
    /// Sound effects requested this frame: (which, gain 0..1, pan -1..1). Drained by the app.
    pub sounds: Vec<(Sfx, f32, f32)>,
    shard_snd_cd: f32,
    /// Age (seconds) of the bullet that last touched the player (balance stats).
    pub last_hit_age: f32,
    /// 1.0 normally; drops after a hit and recovers (see `MERCY_*`).
    pub mercy: f32,
}

pub fn tile_center(c: usize, r: usize) -> Vec2 {
    vec2(
        -HALF_W + (c as f32 + 0.5) * TILE,
        -HALF_H + (r as f32 + 0.5) * TILE,
    )
}

pub fn tile_of(p: Vec2) -> Option<(usize, usize)> {
    let c = ((p.x + HALF_W) / TILE).floor();
    let r = ((p.y + HALF_H) / TILE).floor();
    if c >= 0.0 && r >= 0.0 && (c as usize) < COLS && (r as usize) < ROWS {
        Some((c as usize, r as usize))
    } else {
        None
    }
}

fn dir(a: f32) -> Vec2 {
    vec2(a.cos(), a.sin())
}

impl World {
    pub fn new(diff: f32, no_fail: bool) -> Self {
        World {
            t: 0.0,
            player: Player::new(),
            heart: Heart { hp: 100.0, max_hp: 100.0, broken: false, punch: 0.0, flash: 0.0, rot: 0.0, reform: 0.0, charge: 0.0 },
            bullets: Vec::with_capacity(MAX_BULLETS),
            shots: Vec::with_capacity(128),
            enemies: Vec::with_capacity(MAX_ENEMIES),
            portals: Vec::new(),
            lasers: Vec::new(),
            waves: Vec::new(),
            shards: Vec::with_capacity(256),
            orbs: Vec::new(),
            parts: Vec::with_capacity(MAX_PARTS),
            texts: Vec::new(),
            tiles: vec![Tile { strike_t: -1.0, lead: 1.0, armed: false, flash: 0.0, tag: 0 }; COLS * ROWS],
            pulses: Vec::with_capacity(16),
            hit: [0.0; 3],
            beat_phase: 0.0,
            beat_len: 0.5,
            groove: 1.0,
            groove_amt: 0.28,
            warp: 0.0,
            threat: 0.0,
            shake: 0.0,
            flash: 0.0,
            beat_pulse: 0.0,
            bar_pulse: 0.0,
            beat_count: 0,
            score: 0,
            combo: 0.0,
            combo_idle: 0.0,
            stats: Stats::default(),
            section_kind: SectionKind::Normal,
            section_idx: 0,
            section_banner: 0.0,
            palette: 0,
            intensity: 0.0,
            feat: Feat::default(),
            rng: Rng::new(0x51AB_C0DE),
            no_fail,
            diff,
            ended: false,
            dead: false,
            song_idx: 0,
            pending_orbs: 0,
            active_t: 0.0,
            sounds: Vec::with_capacity(16),
            shard_snd_cd: 0.0,
            last_hit_age: 0.0,
            mercy: 1.0,
        }
    }

    /// Current difficulty multiplier applied to the planned level (about 0.2 .. 1.3).
    pub fn pressure(&self) -> f32 {
        let (r0, secs) = RAMP[(self.song_idx as usize).min(RAMP.len() - 1)];
        let k = smooth01(self.active_t / secs);
        (r0 + (1.0 - r0) * k) * (1.0 + RUN_ESCALATION * self.song_idx as f32) * self.mercy
    }

    /// Reset per-song state; player upgrades, HP and score carry over.
    pub fn begin_song(&mut self, active_secs: f32, song_idx: u32, seed: u64) {
        self.t = 0.0;
        self.bullets.clear();
        self.shots.clear();
        self.enemies.clear();
        self.portals.clear();
        self.lasers.clear();
        self.waves.clear();
        self.shards.clear();
        self.pulses.clear();
        self.hit = [0.0; 3];
        self.warp = 1.0;
        self.active_t = 0.0;
        self.mercy = 1.0;
        for t in self.tiles.iter_mut() {
            *t = Tile { strike_t: -1.0, lead: 1.0, armed: false, flash: 0.0, tag: 0 };
        }
        self.song_idx = song_idx;
        self.rng = Rng::new(seed ^ 0xA5A5_5A5A);
        let hp = active_secs * 2.6 * (1.0 + 0.3 * song_idx as f32) * (0.75 + 0.25 * self.diff);
        self.heart = Heart { hp, max_hp: hp, broken: false, punch: 0.0, flash: 0.0, rot: 0.0, reform: 1.0, charge: 0.0 };
        self.ended = false;
        self.dead = false;
        self.beat_count = 0;
        self.section_banner = 0.0;
        self.player.fire_phase = 0;
        if self.player.up(UpgradeId::Encore) > 0 {
            self.player.shield = true;
        }
    }

    fn snd(&mut self, s: Sfx, gain: f32, x: f32) {
        if self.sounds.len() < 16 {
            self.sounds.push((s, gain, (x / HALF_W).clamp(-1.0, 1.0)));
        }
    }

    pub fn mult(&self) -> f32 {
        (1.0 + (self.combo / 8.0).floor() * 0.5).min(8.0)
    }

    fn add_combo(&mut self, v: f32) {
        self.combo += v;
        self.combo_idle = 0.0;
        if self.combo > self.stats.max_combo {
            self.stats.max_combo = self.combo;
        }
    }

    fn add_score(&mut self, base: f32) {
        self.score += (base * self.mult()).round().max(0.0) as u64;
    }

    pub fn burst(&mut self, pos: Vec2, n: usize, speed: f32, col: u8, size: f32) {
        for _ in 0..n {
            if self.parts.len() >= MAX_PARTS {
                break;
            }
            let a = self.rng.angle();
            let s = speed * self.rng.range(0.25, 1.0);
            let life = self.rng.range(0.25, 0.7);
            self.parts.push(Particle {
                pos,
                vel: dir(a) * s,
                life,
                max: life,
                size: size * self.rng.range(0.5, 1.2),
                col,
                rot: a,
            });
        }
    }

    pub fn text(&mut self, pos: Vec2, s: &str, col: u8, size: f32, life: f32) {
        self.texts.push(FloatText { pos, text: s.to_string(), life, max: life, col, size });
    }

    fn drop_shards(&mut self, pos: Vec2, n: usize, heal: bool) {
        for k in 0..n {
            let a = self.rng.angle();
            let s = self.rng.range(60.0, 220.0);
            self.shards.push(Shard { pos, vel: dir(a) * s, life: 9.0, value: 1, heal: heal && k == 0, age: 0.0 });
        }
    }

    // -----------------------------------------------------------------------------------------
    // Level events
    // -----------------------------------------------------------------------------------------

    pub fn apply(&mut self, e: &LevelEvent) {
        let late = (self.t - e.t).clamp(0.0, 0.25);
        let p = self.pressure();
        match e.ev {
            Ev::Beat { down } => self.on_beat(down),
            Ev::Hit { band, strength } => {
                let b = (band as usize).min(2);
                self.hit[b] = self.hit[b].max(0.45 + 0.55 * strength.clamp(0.0, 1.0));
                if b == 0 {
                    self.heart.punch = self.heart.punch.max(0.6);
                }
            }
            Ev::Pulse { pattern, count, speed, rot, soft } => {
                // Scale the planned pulse by the current pressure.
                let count = if count == 0 { 0 } else { ((count as f32 * p).round() as u16).clamp(3, 64) };
                let speed = speed * (0.82 + 0.18 * p.min(1.25));
                if e.lead > 0.02 && self.t < e.t - 0.01 {
                    if self.pulses.len() < 24 {
                        let aim = self.player.pos.y.atan2(self.player.pos.x);
                        self.pulses.push(PendingPulse { t: e.t, lead: e.lead, pattern, count, speed, rot, soft, aim });
                    }
                } else {
                    let aim = self.player.pos.y.atan2(self.player.pos.x);
                    self.emit_pulse(pattern, count as usize, speed, rot, soft, late, aim);
                }
            }
            Ev::Tick { speed } => {
                if p >= P_TICK {
                    self.emit_tick(speed, late);
                }
            }
            Ev::Spawn { kind, x, y } => {
                let cap = (2.0 + 5.0 * p * self.diff).round() as usize;
                if p >= P_SPAWN && self.enemies.len() + self.portals.len() < cap.min(MAX_ENEMIES) {
                    // Tougher kinds only appear once the run has warmed up.
                    let kind = match kind {
                        EnemyKind::Elite if p < P_ELITE => EnemyKind::Orbiter,
                        EnemyKind::Splitter if p < 0.8 => EnemyKind::Chaser,
                        EnemyKind::Darter if p < 0.7 => EnemyKind::Chaser,
                        EnemyKind::Orbiter if p < 0.6 => EnemyKind::Chaser,
                        k => k,
                    };
                    // Never open a portal right on top of the player.
                    let mut pos = vec2(x, y);
                    if pos.distance(self.player.pos) < 230.0 {
                        pos = -pos;
                    }
                    self.portals.push(Portal { kind, pos, t0: e.at(), t1: e.t });
                }
            }
            Ev::Shards { x, y, n } => {
                for _ in 0..n {
                    let j = vec2(self.rng.range(-14.0, 14.0), self.rng.range(-14.0, 14.0));
                    self.shards.push(Shard { pos: vec2(x, y) + j, vel: Vec2::ZERO, life: 7.0, value: 1, heal: false, age: 0.0 });
                }
            }
            Ev::Laser { horizontal, from, to, dur } => {
                if p >= P_LASER && self.lasers.len() < 2 {
                    self.lasers.push(Laser { horizontal, from, to, t0: e.t, dur, lead: e.lead.max(0.3) });
                }
            }
            Ev::Tiles { pattern } if p < P_TILES => {
                let _ = pattern;
            }
            Ev::Tiles { pattern } => {
                for r in 0..ROWS {
                    for c in 0..COLS {
                        if tile_in_pattern(pattern, c, r) {
                            let tile = &mut self.tiles[r * COLS + c];
                            if !tile.armed {
                                tile.armed = true;
                                tile.strike_t = e.t;
                                tile.lead = e.lead.max(0.3);
                                tile.tag = tile_tag(pattern);
                            }
                        }
                    }
                }
            }
            Ev::Shockwave { gap, gaps } => {
                if p >= P_WAVE {
                    self.waves.push(Wave { t0: e.t, lead: e.lead.max(0.3), gap, gaps, speed: 430.0 });
                }
            }
            Ev::Section { idx, kind, intensity: _, palette } => {
                self.section_idx = idx;
                self.section_kind = kind;
                self.palette = palette;
                self.section_banner = 2.4;
                self.heart.reform = 1.0;
                self.groove_amt = match kind {
                    SectionKind::Calm => 0.15,
                    SectionKind::Normal => 0.28,
                    SectionKind::Intense => 0.38,
                    SectionKind::Drop => 0.45,
                };
                if self.player.up(UpgradeId::Encore) > 0 {
                    self.player.shield = true;
                }
                if kind == SectionKind::Drop {
                    self.flash = self.flash.max(0.35);
                }
            }
            Ev::Orbs => self.spawn_orbs(14.0),
            Ev::Heal { x, y } => {
                self.shards.push(Shard { pos: vec2(x, y), vel: Vec2::ZERO, life: 12.0, value: 2, heal: true, age: 0.0 });
            }
            Ev::End => {
                if !self.ended {
                    self.ended = true;
                    let bonus = 2000.0 + self.player.hp.max(0) as f32 * 500.0;
                    self.score += bonus as u64;
                    self.stats.songs += 1;
                    self.snd(Sfx::Clear, 0.9, 0.0);
                }
            }
        }
    }

    fn on_beat(&mut self, down: bool) {
        self.beat_pulse = 1.0;
        if down {
            self.bar_pulse = 1.0;
        }
        self.beat_count += 1;
        // Enemy fire cadence is set in seconds and snapped to whole beats, so tempo does not
        // change how often they shoot.
        let every = |secs: f32| -> u32 { (secs / self.beat_len.max(0.2)).round().max(1.0) as u32 };
        let even = self.beat_count % every(1.2) == 0;
        let elite_fire = self.beat_count % every(2.2) == 0;
        let ppos = self.player.pos;
        let speed = 210.0 + 90.0 * self.intensity;
        let mut new_bullets: Vec<Bullet> = Vec::new();
        for en in self.enemies.iter_mut() {
            match en.kind {
                EnemyKind::Orbiter if even => {
                    let d = (ppos - en.pos).normalize_or_zero();
                    new_bullets.push(Bullet { pos: en.pos + d * en.r, vel: d * speed, r: 7.0, soft: false, age: 0.0, fade: 1.0, kind: B_ORB, pat: 10 });
                    en.flash = 0.6;
                }
                EnemyKind::Elite if elite_fire => {
                    let base = en.age;
                    for k in 0..8 {
                        let d = dir(base + k as f32 * TAU / 8.0);
                        let soft = k % 2 == 0;
                        new_bullets.push(Bullet { pos: en.pos + d * en.r, vel: d * speed * 0.85, r: 7.0, soft, age: 0.0, fade: 1.0, kind: if soft { B_NOTE } else { B_ORB }, pat: 11 });
                    }
                    en.flash = 0.6;
                }
                _ => {}
            }
        }
        if self.bullets.len() + new_bullets.len() < MAX_BULLETS {
            self.bullets.extend(new_bullets);
        }
        // Echo drones fire on every beat at the nearest target.
        let n = self.player.drones();
        let dmg = (1.0 + 0.5 * self.player.up(UpgradeId::Gain) as f32) * (self.beat_len / 0.5).clamp(0.5, 2.0);
        for k in 0..n {
            let from = self.player.drone_pos(k);
            let mut target = if self.heart.broken { None } else { Some(Vec2::ZERO) };
            let mut best = 520.0f32;
            for en in &self.enemies {
                let d = en.pos.distance(from);
                if d < best {
                    best = d;
                    target = Some(en.pos);
                }
            }
            if let Some(tp) = target {
                let d = (tp - from).normalize_or_zero();
                self.shots.push(Shot { pos: from, vel: d * 1150.0, dmg, pierce: 0, life: 0.8, r: 4.0 });
            }
        }
    }

    fn emit_pulse(&mut self, pattern: Pattern, mut count: usize, speed: f32, rot: f32, mut soft: bool, late: f32, aim: f32) {
        self.heart.punch = 1.0;
        if count == 0 {
            return;
        }
        if self.heart.broken {
            count = (count / 2).max(2);
            soft = true;
        }
        if self.bullets.len() + count > MAX_BULLETS {
            return;
        }
        let r = if soft { 8.0 } else { 7.0 };
        let kind = if soft { B_NOTE } else { B_ORB };
        let pat = pattern as u8;
        let push = |b: &mut Vec<Bullet>, a: f32, sp: f32| {
            let d = dir(a);
            let vel = d * sp;
            b.push(Bullet { pos: d * (HEART_R * 0.85) + vel * late, vel, r, soft, age: 0.0, fade: 1.0, kind, pat });
        };
        let b = &mut self.bullets;
        match pattern {
            Pattern::Ring => {
                for k in 0..count {
                    push(b, rot + k as f32 * TAU / count as f32, speed);
                }
            }
            Pattern::Fan => {
                let n = count.max(1) | 1;
                for k in 0..n {
                    push(b, aim + (k as f32 - (n as f32 - 1.0) * 0.5) * FAN_STEP, speed);
                }
            }
            Pattern::Spiral => {
                let arms = 2usize;
                let per = (count / arms).max(1);
                for a in 0..arms {
                    for j in 0..per {
                        push(b, rot + a as f32 * TAU / arms as f32 + j as f32 * 0.12, speed * (1.0 - 0.09 * j as f32).max(0.4));
                    }
                }
            }
            Pattern::Cross => {
                let per = (count / 4).max(1);
                for a in 0..4 {
                    for j in 0..per {
                        push(b, aim + a as f32 * TAU / 4.0, speed * (1.0 - 0.11 * j as f32).max(0.35));
                    }
                }
            }
            Pattern::Wall => {
                // Dense arc centred on the player with one gap at `aim + rot`.
                let step = 2.0 * WALL_HALF / (count.max(2) - 1) as f32;
                for k in 0..count {
                    let off = -WALL_HALF + k as f32 * step;
                    if (off - rot).abs() < WALL_GAP_HALF {
                        continue;
                    }
                    push(b, aim + off, speed);
                }
            }
            Pattern::Burst => {
                for j in 0..count {
                    push(b, aim, speed * (1.0 - 0.085 * j as f32).max(0.4));
                }
            }
            Pattern::Pincer => {
                let per = (count / 2).max(1);
                for side in [-1.0f32, 1.0] {
                    for k in 0..per {
                        let f = if per > 1 { k as f32 / (per - 1) as f32 - 0.5 } else { 0.0 };
                        push(b, aim + side * PINCER_OFFSET + f * PINCER_SPREAD, speed);
                    }
                }
            }
            Pattern::Flower => {
                for k in 0..count {
                    let a = k as f32 * TAU / count as f32;
                    push(b, rot + a, speed * (0.78 + 0.22 * (5.0 * a).cos()));
                }
            }
            Pattern::Arc => {
                let span = TAU * 0.74;
                for k in 0..count {
                    let f = if count > 1 { k as f32 / (count - 1) as f32 } else { 0.5 };
                    push(b, rot + TAU * 0.13 + f * span, speed);
                }
            }
        }
    }

    /// One small fast dart from the Heart aimed at where the player is (hi-hat streams).
    fn emit_tick(&mut self, speed: f32, late: f32) {
        if self.heart.broken || self.bullets.len() >= MAX_BULLETS {
            return;
        }
        let d = self.player.pos.normalize_or_zero();
        if d == Vec2::ZERO {
            return;
        }
        let vel = d * speed;
        self.bullets.push(Bullet { pos: d * (HEART_R * 0.9) + vel * late, vel, r: 4.5, soft: false, age: 0.0, fade: 1.0, kind: B_TICK, pat: 9 });
    }

    pub fn spawn_orbs(&mut self, life: f32) {
        if !self.orbs.is_empty() {
            self.pending_orbs += 1;
            return;
        }
        let mut pool: Vec<UpgradeId> = UpgradeId::ALL
            .iter()
            .copied()
            .filter(|u| self.player.up(*u) < u.max())
            .collect();
        if pool.is_empty() {
            self.drop_shards(vec2(0.0, 250.0), 10, true);
            return;
        }
        let base = self.rng.angle();
        for k in 0..3 {
            if pool.is_empty() {
                break;
            }
            let i = self.rng.below(pool.len());
            let up = pool.swap_remove(i);
            let a = base + k as f32 * TAU / 3.0;
            let pos = vec2(a.cos() * 430.0, a.sin() * 270.0);
            self.orbs.push(Orb { pos, up, life, age: 0.0 });
        }
        let p = self.player.pos + vec2(0.0, -60.0);
        self.text(p, "UPGRADE READY", C_ACCENT, 26.0, 1.6);
    }

    fn apply_upgrade(&mut self, up: UpgradeId) {
        let p = &mut self.player;
        p.ups[up as usize] = (p.ups[up as usize] + 1).min(up.max());
        match up {
            UpgradeId::Sustain => {
                p.max_hp += 1;
                p.hp = (p.hp + 2).min(p.max_hp);
            }
            UpgradeId::Phase => p.dash_cd_max = 1.1 * 0.75f32.powi(p.ups[up as usize] as i32),
            UpgradeId::Encore => p.shield = true,
            _ => {}
        }
        // Every upgrade also patches you up a little.
        if up != UpgradeId::Sustain && self.player.hp < self.player.max_hp {
            self.player.hp += 1;
        }
        let pos = self.player.pos;
        self.text(pos + vec2(0.0, -50.0), up.name(), C_ACCENT, 34.0, 1.8);
        self.text(pos + vec2(0.0, -22.0), up.desc(), C_WHITE, 18.0, 1.8);
        self.burst(pos, 26, 420.0, C_ACCENT, 5.0);
        self.flash = self.flash.max(0.15);
        self.snd(Sfx::Upgrade, 0.9, pos.x);
    }

    fn hurt_player(&mut self, src: usize) {
        let p = &mut self.player;
        if p.invuln > 0.0 || p.dash_t > 0.0 || self.dead {
            return;
        }
        if p.shield {
            p.shield = false;
            p.invuln = 0.9;
            let pos = p.pos;
            self.burst(pos, 18, 380.0, C_PRIMARY, 4.0);
            self.text(pos + vec2(0.0, -40.0), "ENCORE", C_PRIMARY, 22.0, 1.0);
            self.shake = self.shake.max(0.3);
            self.snd(Sfx::Shield, 0.8, pos.x);
            return;
        }
        p.hp -= 1;
        p.invuln = HIT_INVULN;
        let pos = p.pos;
        self.stats.hits += 1;
        self.stats.by_src[src.min(5)] += 1;
        self.snd(Sfx::Hurt, 1.0, pos.x);
        self.mercy = MERCY_DROP;
        self.combo *= 0.5;
        self.shake = self.shake.max(0.7);
        self.flash = self.flash.max(0.45);
        self.burst(pos, 22, 460.0, C_DANGER, 5.0);
        // Mercy: clear nearby bullets and shove enemies away.
        for b in self.bullets.iter_mut() {
            if b.pos.distance(pos) < HIT_CLEAR_R {
                b.fade = b.fade.min(0.99);
            }
        }
        for en in self.enemies.iter_mut() {
            let d = en.pos - pos;
            let l = d.length();
            if l < HIT_CLEAR_R && l > 0.01 {
                en.vel = d / l * 420.0;
                en.pos += d / l * (HIT_CLEAR_R - l) * 0.5;
            }
        }
        if self.player.hp <= 0 {
            if self.no_fail {
                self.player.hp = self.player.max_hp;
                self.score = (self.score as f64 * 0.85) as u64;
                self.combo = 0.0;
                self.text(pos + vec2(0.0, -46.0), "REBOOT  -15% SCORE", C_DANGER, 24.0, 1.8);
            } else {
                self.dead = true;
                self.burst(pos, 60, 700.0, C_WHITE, 6.0);
                self.snd(Sfx::Death, 1.0, pos.x);
            }
        }
    }

    fn make_enemy(&mut self, kind: EnemyKind, pos: Vec2) {
        let (hp, r) = match kind {
            EnemyKind::Chaser => (2.0, 15.0),
            EnemyKind::Darter => (1.0, 13.0),
            EnemyKind::Orbiter => (5.0, 17.0),
            EnemyKind::Splitter => (5.0, 22.0),
            EnemyKind::Mini => (1.0, 9.0),
            EnemyKind::Elite => (22.0 + 8.0 * self.diff + 6.0 * self.song_idx as f32, 34.0),
        };
        let mut vel = Vec2::ZERO;
        let mut state = 0.0;
        match kind {
            EnemyKind::Darter => {
                let d = (self.player.pos - pos).normalize_or_zero();
                vel = d * 640.0;
            }
            EnemyKind::Orbiter => state = pos.y.atan2(pos.x / 1.1),
            _ => {}
        }
        self.enemies.push(Enemy { kind, pos, vel, hp, max_hp: hp, r, rot: 0.0, age: 0.0, flash: 0.0, state });
    }

    fn kill_enemy(&mut self, kind: EnemyKind, pos: Vec2) {
        let (score, shards, col) = match kind {
            EnemyKind::Chaser => (50.0, 2, C_SECONDARY),
            EnemyKind::Darter => (40.0, 1, C_ACCENT),
            EnemyKind::Orbiter => (120.0, 3, C_PRIMARY),
            EnemyKind::Splitter => (150.0, 3, C_SECONDARY),
            EnemyKind::Mini => (20.0, 1, C_SECONDARY),
            EnemyKind::Elite => (600.0, 9, C_DANGER),
        };
        self.add_score(score);
        self.add_combo(1.0);
        self.stats.kills += 1;
        self.snd(Sfx::Kill, if kind == EnemyKind::Elite { 1.0 } else { 0.55 }, pos.x);
        self.drop_shards(pos, shards, kind == EnemyKind::Elite);
        self.burst(pos, 10 + shards * 4, 360.0, col, 5.0);
        if kind == EnemyKind::Elite {
            self.shake = self.shake.max(0.5);
        }
        if kind == EnemyKind::Splitter {
            for k in 0..3 {
                let a = k as f32 * TAU / 3.0 + self.rng.angle() * 0.2;
                if self.enemies.len() < MAX_ENEMIES + 6 {
                    self.make_enemy(EnemyKind::Mini, pos + dir(a) * 18.0);
                    if let Some(m) = self.enemies.last_mut() {
                        m.vel = dir(a) * 260.0;
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Frame update
    // -----------------------------------------------------------------------------------------

    /// `beat_pos` is the continuous beat index at time `t`; `active` is false in drift mode.
    pub fn update(&mut self, dt: f32, t: f32, input: &Input, beat_pos: f32, feat: &Feat, intensity: f32, active: bool) {
        self.t = t;
        self.feat = *feat;
        self.intensity = intensity;
        if active {
            self.active_t += dt;
        }
        self.mercy = (self.mercy + dt * MERCY_RECOVER).min(1.0);
        self.threat = self.pressure().min(1.0);

        // ---- decays ----
        self.beat_pulse = (self.beat_pulse - dt * 4.5).max(0.0);
        self.bar_pulse = (self.bar_pulse - dt * 2.2).max(0.0);
        self.shake = (self.shake - dt * 2.4).max(0.0);
        self.flash = (self.flash - dt * 2.0).max(0.0);
        self.section_banner = (self.section_banner - dt).max(0.0);
        self.warp = (self.warp - dt * 0.9).max(0.0);
        for h in self.hit.iter_mut() {
            *h = (*h - dt * HIT_DECAY).max(0.0);
        }
        // Groove: everything hostile surges on the beat and eases off before the next one.
        self.beat_phase = beat_pos.rem_euclid(1.0);
        self.groove = if active {
            1.0 + self.groove_amt * (GROOVE_PEAK * (-GROOVE_DECAY * self.beat_phase).exp() - GROOVE_MEAN)
        } else {
            1.0
        };
        // Announced pulses fire exactly on their kick.
        let mut pi = 0;
        while pi < self.pulses.len() {
            if t >= self.pulses[pi].t {
                let p = self.pulses.swap_remove(pi);
                if active {
                    self.emit_pulse(p.pattern, p.count as usize, p.speed, p.rot, p.soft, (t - p.t).clamp(0.0, 0.25), p.aim);
                }
            } else {
                pi += 1;
            }
        }
        self.heart.punch = (self.heart.punch - dt * 6.0).max(0.0);
        self.heart.flash = (self.heart.flash - dt * 8.0).max(0.0);
        self.heart.reform = (self.heart.reform - dt * 0.9).max(0.0);
        self.heart.rot += dt * (0.25 + 0.9 * intensity);
        self.combo_idle += dt;
        if self.combo_idle > 3.0 {
            self.combo = (self.combo - dt * (0.8 + self.combo * 0.05)).max(0.0);
        }

        if !self.dead {
            self.update_player(dt, input, beat_pos, active);
        }

        // ---- shots ----
        let mut i = 0;
        while i < self.shots.len() {
            let s = &mut self.shots[i];
            s.pos += s.vel * dt;
            s.life -= dt;
            let out = s.pos.x.abs() > HALF_W + 60.0 || s.pos.y.abs() > HALF_H + 60.0;
            if s.life <= 0.0 || out {
                self.shots.swap_remove(i);
            } else {
                i += 1;
            }
        }
        self.collide_shots();

        // ---- portals -> enemies ----
        let mut k = 0;
        while k < self.portals.len() {
            if t >= self.portals[k].t1 {
                let p = self.portals.swap_remove(k);
                self.make_enemy(p.kind, p.pos);
                self.burst(p.pos, 8, 260.0, C_SECONDARY, 4.0);
            } else {
                k += 1;
            }
        }

        self.update_enemies(dt, active);
        self.update_bullets(dt, active);
        self.update_hazards(dt, t, active);
        self.update_pickups(dt);

        // ---- particles / texts ----
        for p in self.parts.iter_mut() {
            p.pos += p.vel * dt;
            p.vel *= 1.0 - (3.0 * dt).min(0.9);
            p.life -= dt;
        }
        self.parts.retain(|p| p.life > 0.0);
        for tx in self.texts.iter_mut() {
            tx.pos.y -= 26.0 * dt;
            tx.life -= dt;
        }
        self.texts.retain(|t| t.life > 0.0);
    }

    fn update_player(&mut self, dt: f32, input: &Input, beat_pos: f32, active: bool) {
        let glide = self.player.up(UpgradeId::Glide) as f32;
        let p = &mut self.player;
        p.invuln = (p.invuln - dt).max(0.0);
        p.dash_cd = (p.dash_cd - dt).max(0.0);
        p.muzzle = (p.muzzle - dt * 9.0).max(0.0);
        p.drone_ang += dt * 2.4;
        let aim = input.aim - p.pos;
        if aim.length_squared() > 4.0 {
            p.aim_dir = aim.normalize();
        }
        let mv = if input.mv.length_squared() > 1.0 { input.mv.normalize() } else { input.mv };
        let mut dashed = false;
        if input.dash && p.dash_cd <= 0.0 && p.dash_t <= 0.0 {
            p.dash_t = 0.16;
            p.dash_cd = p.dash_cd_max;
            p.dash_dir = if mv.length_squared() > 0.01 { mv.normalize() } else { p.aim_dir };
            dashed = true;
        }
        if dashed {
            let x = p.pos.x;
            self.snd(Sfx::Dash, 0.5, x);
        }
        let p = &mut self.player;
        let mut spawn_after = false;
        if p.dash_t > 0.0 {
            p.dash_t -= dt;
            p.vel = p.dash_dir * 1450.0;
            p.invuln = p.invuln.max(0.1);
            spawn_after = true;
        } else {
            let speed = 430.0 * (1.0 + 0.12 * glide);
            let target = mv * speed;
            let k = (16.0 * dt).min(1.0);
            p.vel += (target - p.vel) * k;
        }
        p.pos += p.vel * dt;
        p.pos.x = p.pos.x.clamp(-HALF_W + PLAYER_R, HALF_W - PLAYER_R);
        p.pos.y = p.pos.y.clamp(-HALF_H + PLAYER_R, HALF_H - PLAYER_R);
        // The Heart is solid.
        let d = p.pos.length();
        let min_d = HEART_R + PLAYER_R;
        let mut touched = false;
        if d < min_d {
            let n = if d > 0.01 { p.pos / d } else { vec2(0.0, 1.0) };
            p.pos = n * min_d;
            touched = true;
        }
        let pos = p.pos;
        if spawn_after && self.parts.len() < MAX_PARTS {
            self.parts.push(Particle { pos, vel: Vec2::ZERO, life: 0.22, max: 0.22, size: PLAYER_R * 1.6, col: C_PRIMARY, rot: 0.0 });
        }
        if touched && active && !self.heart.broken {
            self.hurt_player(SRC_HEART);
        }

        // ---- beat-quantised gun ----
        // The gun always fires on a subdivision of the beat, but which one is chosen so the
        // fire rate (and with the damage correction, the DPS) is the same for every tempo.
        let lv = self.player.up(UpgradeId::Tempo) as usize;
        let target = FIRE_INTERVALS[lv.min(FIRE_INTERVALS.len() - 1)];
        let bl = self.beat_len.clamp(0.2, 2.0);
        let mut div = 1.0f32;
        let mut best = f32::MAX;
        for &d in FIRE_DIVS.iter() {
            let err = ((bl / d) / target).ln().abs();
            if err < best {
                best = err;
                div = d;
            }
        }
        let idx = (beat_pos * div).floor() as i64;
        if idx != self.player.fire_phase {
            self.player.fire_phase = idx;
            if input.fire {
                self.fire(((bl / div) / target).clamp(0.6, 1.7));
            }
        }
    }

    fn fire(&mut self, dmg_scale: f32) {
        let p = &mut self.player;
        let n = 1 + p.ups[UpgradeId::Chord as usize] as usize;
        let dmg = (1.0 + 0.5 * p.ups[UpgradeId::Gain as usize] as f32) * dmg_scale;
        let pierce = p.ups[UpgradeId::Pierce as usize];
        let base = p.aim_dir.y.atan2(p.aim_dir.x);
        p.muzzle = 1.0;
        for k in 0..n {
            let off = (k as f32 - (n as f32 - 1.0) * 0.5) * 0.11;
            let d = dir(base + off);
            self.shots.push(Shot { pos: p.pos + d * 15.0, vel: d * 1250.0, dmg, pierce, life: 0.85, r: 5.0 });
        }
    }

    fn collide_shots(&mut self) {
        let mut killed: Vec<(EnemyKind, Vec2)> = Vec::new();
        let mut heart_dmg = 0.0f32;
        let mut sparks: Vec<(Vec2, u8)> = Vec::new();
        let mut notes: Vec<Vec2> = Vec::new();
        let mut si = 0;
        'shots: while si < self.shots.len() {
            let (sp, sr, dmg) = {
                let s = &self.shots[si];
                (s.pos, s.r, s.dmg)
            };
            // Heart
            if !self.heart.broken && sp.length() < HEART_R + sr {
                heart_dmg += dmg;
                sparks.push((sp, C_WHITE));
                self.shots.swap_remove(si);
                continue 'shots;
            }
            // Enemies
            let mut consumed = false;
            for ei in 0..self.enemies.len() {
                let en = &mut self.enemies[ei];
                if en.pos.distance(sp) < en.r + sr + 2.0 {
                    en.hp -= dmg;
                    en.flash = 1.0;
                    sparks.push((sp, C_WHITE));
                    if en.hp <= 0.0 {
                        let dead = self.enemies.swap_remove(ei);
                        killed.push((dead.kind, dead.pos));
                    }
                    let s = &mut self.shots[si];
                    if s.pierce > 0 {
                        s.pierce -= 1;
                        // Nudge forward so the same target is not hit again next frame.
                        s.pos += s.vel.normalize_or_zero() * 34.0;
                    } else {
                        consumed = true;
                    }
                    break;
                }
            }
            if consumed {
                self.shots.swap_remove(si);
                continue 'shots;
            }
            // Soft (destructible) bullets
            for bi in 0..self.bullets.len() {
                let b = &self.bullets[bi];
                if b.soft && b.fade >= 1.0 && b.pos.distance(sp) < b.r + sr + 5.0 {
                    notes.push(b.pos);
                    self.bullets.swap_remove(bi);
                    let s = &mut self.shots[si];
                    if s.pierce > 0 {
                        s.pierce -= 1;
                    } else {
                        self.shots.swap_remove(si);
                        continue 'shots;
                    }
                    break;
                }
            }
            si += 1;
        }
        for (pos, col) in sparks {
            self.burst(pos, 3, 240.0, col, 3.0);
        }
        if !notes.is_empty() {
            let x = notes[0].x;
            self.snd(Sfx::NoteShot, 0.5, x);
        }
        for pos in notes {
            self.stats.notes_shot += 1;
            self.add_score(15.0);
            self.add_combo(0.25);
            self.burst(pos, 5, 220.0, C_WHITE, 3.5);
            if self.rng.chance(0.4) {
                self.drop_shards(pos, 1, false);
            }
        }
        for (kind, pos) in killed {
            self.kill_enemy(kind, pos);
        }
        if heart_dmg > 0.0 && !self.heart.broken {
            self.heart.hp -= heart_dmg;
            self.heart.flash = 1.0;
            self.add_score(heart_dmg * 4.0);
            if self.heart.hp <= 0.0 {
                self.heart.hp = 0.0;
                self.heart.broken = true;
                self.stats.hearts_broken += 1;
                self.snd(Sfx::HeartBreak, 1.0, 0.0);
                self.add_score(5000.0);
                self.shake = 1.0;
                self.flash = 0.8;
                self.burst(Vec2::ZERO, 90, 900.0, C_PRIMARY, 8.0);
                self.burst(Vec2::ZERO, 50, 600.0, C_WHITE, 6.0);
                self.text(vec2(0.0, -120.0), "HEART BROKEN", C_ACCENT, 54.0, 3.0);
                self.text(vec2(0.0, -78.0), "victory lap - everything is shootable", C_WHITE, 22.0, 3.0);
                // Every bullet on the field becomes a reward.
                let pts: Vec<Vec2> = self.bullets.iter().map(|b| b.pos).collect();
                self.bullets.clear();
                for (n, p) in pts.into_iter().enumerate() {
                    if n % 3 == 0 {
                        self.drop_shards(p, 1, false);
                    }
                }
            }
        }
    }

    fn update_enemies(&mut self, dt: f32, active: bool) {
        let ppos = self.player.pos;
        let slow = if active { self.groove } else { 0.15 };
        let intensity = self.intensity;
        let n = self.enemies.len();
        // Simple separation so chasers do not stack.
        let positions: Vec<(Vec2, f32)> = self.enemies.iter().map(|e| (e.pos, e.r)).collect();
        let mut touched = false;
        let mut trails: Vec<Vec2> = Vec::new();
        for (i, en) in self.enemies.iter_mut().enumerate() {
            en.age += dt;
            en.flash = (en.flash - dt * 7.0).max(0.0);
            let to_p = ppos - en.pos;
            let dist = to_p.length().max(0.001);
            match en.kind {
                EnemyKind::Chaser | EnemyKind::Mini | EnemyKind::Splitter | EnemyKind::Elite => {
                    let max = match en.kind {
                        EnemyKind::Chaser => 110.0 + 60.0 * intensity,
                        EnemyKind::Mini => 215.0,
                        EnemyKind::Splitter => 88.0,
                        _ => 58.0,
                    };
                    let mut acc = to_p / dist * max * 3.0;
                    for (j, &(op, or)) in positions.iter().enumerate() {
                        if j == i || j >= n {
                            continue;
                        }
                        let d = en.pos - op;
                        let l = d.length();
                        let min = en.r + or + 6.0;
                        if l < min && l > 0.01 {
                            acc += d / l * (min - l) * 30.0;
                        }
                    }
                    en.vel += acc * dt;
                    let sp = en.vel.length();
                    if sp > max {
                        en.vel *= max / sp;
                    }
                    en.pos += en.vel * dt * slow;
                    en.rot = en.vel.y.atan2(en.vel.x);
                    // Stay off the Heart.
                    let hd = en.pos.length();
                    if hd < HEART_R + en.r + 6.0 && hd > 0.01 {
                        en.pos = en.pos / hd * (HEART_R + en.r + 6.0);
                    }
                }
                EnemyKind::Darter => {
                    en.pos += en.vel * dt * slow;
                    en.rot = en.vel.y.atan2(en.vel.x);
                    if (en.age * 60.0) as i32 % 2 == 0 {
                        trails.push(en.pos);
                    }
                    if en.pos.x.abs() > HALF_W + 90.0 || en.pos.y.abs() > HALF_H + 90.0 {
                        en.hp = -1.0; // left the arena
                    }
                }
                EnemyKind::Orbiter => {
                    en.state += dt * 0.55 * slow;
                    let target = vec2(en.state.cos() * 363.0, en.state.sin() * 300.0);
                    en.pos += (target - en.pos) * (3.0 * dt).min(1.0);
                    en.rot += dt * 1.5;
                }
            }
            if active && dist < en.r + PLAYER_HIT_R + 4.0 {
                touched = true;
                if matches!(en.kind, EnemyKind::Mini | EnemyKind::Darter) {
                    en.hp = -1.0;
                }
            }
        }
        for p in trails {
            if self.parts.len() < MAX_PARTS {
                self.parts.push(Particle { pos: p, vel: Vec2::ZERO, life: 0.25, max: 0.25, size: 9.0, col: C_ACCENT, rot: 0.0 });
            }
        }
        self.enemies.retain(|e| e.hp > -0.5);
        if touched {
            self.hurt_player(SRC_ENEMY);
        }
    }

    fn update_bullets(&mut self, dt: f32, active: bool) {
        let ppos = self.player.pos;
        let mut hit = false;
        let mut hit_pat = 0u8;
        let vulnerable = self.player.invuln <= 0.0 && self.player.dash_t <= 0.0 && !self.dead;
        let groove = self.groove;
        for b in self.bullets.iter_mut() {
            // Fresh bullets leave at their plain speed; the beat surge fades in as they travel.
            let g = 1.0 + (groove - 1.0) * (b.age / 0.35).min(1.0);
            b.pos += b.vel * dt * g;
            b.age += dt;
            if !active {
                b.fade = b.fade.min(0.99);
            }
            if b.fade < 1.0 {
                b.fade -= dt * 3.5;
                continue;
            }
            if b.pos.x.abs() > HALF_W + 140.0 || b.pos.y.abs() > HALF_H + 140.0 {
                b.fade = -1.0;
                continue;
            }
            if vulnerable && b.pos.distance_squared(ppos) < (b.r + PLAYER_HIT_R) * (b.r + PLAYER_HIT_R) {
                hit = true;
                hit_pat = b.pat;
                self.last_hit_age = b.age;
                b.fade = -1.0;
            }
        }
        self.bullets.retain(|b| b.fade > 0.0);
        if hit {
            let before = self.stats.hits;
            self.hurt_player(SRC_BULLET);
            if self.stats.hits > before {
                self.stats.by_pat[(hit_pat as usize).min(11)] += 1;
            }
        }
    }

    fn update_hazards(&mut self, dt: f32, t: f32, active: bool) {
        let ppos = self.player.pos;
        let mut hurt: Option<usize> = None;
        let mut tile_tag_hit = 0u8;
        let mut slammed = false;

        // Tiles
        let ptile = tile_of(ppos);
        for r in 0..ROWS {
            for c in 0..COLS {
                let tile = &mut self.tiles[r * COLS + c];
                tile.flash = (tile.flash - dt * 3.5).max(0.0);
                if tile.armed && t >= tile.strike_t {
                    tile.armed = false;
                    // Ignore strikes that are badly stale (e.g. after a hitch).
                    if t - tile.strike_t < 0.3 {
                        tile.flash = 1.0;
                        slammed = true;
                        if active && ptile == Some((c, r)) {
                            tile_tag_hit = tile.tag;
                            hurt = Some(SRC_TILE);
                        }
                    }
                }
            }
        }

        if slammed && active {
            self.snd(Sfx::Slam, 0.5, 0.0);
        }

        // Lasers
        for l in &self.lasers {
            if active && l.active(t) {
                let pos = l.pos_at(t);
                let d = if l.horizontal { ppos.y - pos } else { ppos.x - pos };
                if d.abs() < 13.0 + PLAYER_HIT_R {
                    hurt = Some(SRC_LASER);
                }
            }
        }
        self.lasers.retain(|l| t < l.t0 + l.dur + 0.3 && t > l.t0 - l.lead - 1.0);

        // Shockwaves
        self.heart.charge = 0.0;
        for w in &self.waves {
            if t < w.t0 {
                self.heart.charge = self.heart.charge.max(((t - (w.t0 - w.lead)) / w.lead).clamp(0.0, 1.0));
            } else if active {
                let rad = w.radius(t);
                let d = ppos.length();
                if (d - rad).abs() < 12.0 + PLAYER_HIT_R && !w.in_gap(ppos.y.atan2(ppos.x)) {
                    hurt = Some(SRC_WAVE);
                }
            }
        }
        self.waves.retain(|w| w.radius(t) < 980.0);
        if let Some(src) = hurt {
            let before = self.stats.hits;
            self.hurt_player(src);
            if src == SRC_TILE && self.stats.hits > before {
                self.stats.by_tile[(tile_tag_hit as usize).min(8)] += 1;
            }
        }
    }

    fn update_pickups(&mut self, dt: f32) {
        let ppos = self.player.pos;
        let mag = self.player.magnet_r();
        let mut got = 0u32;
        let mut healed = false;
        for s in self.shards.iter_mut() {
            s.age += dt;
            s.life -= dt;
            let to = ppos - s.pos;
            let d = to.length().max(0.001);
            if d < mag && !self.dead {
                let pull = 2400.0 * (1.0 - d / mag).max(0.15);
                s.vel += to / d * pull * dt;
            }
            s.vel *= 1.0 - (2.2 * dt).min(0.9);
            s.pos += s.vel * dt;
            if d < 22.0 && !self.dead {
                s.life = -1.0;
                got += s.value as u32;
                healed |= s.heal;
            }
        }
        self.shards.retain(|s| s.life > 0.0);
        self.shard_snd_cd = (self.shard_snd_cd - dt).max(0.0);
        if got > 0 && self.shard_snd_cd <= 0.0 {
            self.shard_snd_cd = 0.07;
            self.snd(Sfx::Shard, 0.45, ppos.x);
        }
        if got > 0 {
            self.stats.shards += got;
            self.player.xp += got;
            self.add_score(10.0 * got as f32);
            self.add_combo(0.34 * got as f32);
            if self.parts.len() < MAX_PARTS {
                self.burst(ppos, 2, 160.0, C_ACCENT, 3.0);
            }
        }
        if healed && self.player.hp < self.player.max_hp {
            self.player.hp += 1;
            self.text(ppos + vec2(0.0, -40.0), "+1 HP", C_PRIMARY, 24.0, 1.2);
            self.snd(Sfx::Heal, 0.8, ppos.x);
        }
        if self.player.xp >= self.player.xp_next {
            self.player.xp -= self.player.xp_next;
            self.player.level += 1;
            self.player.xp_next = 14 + 9 * self.player.level;
            self.spawn_orbs(14.0);
        }

        // Upgrade orbs
        let mut taken: Option<UpgradeId> = None;
        for o in self.orbs.iter_mut() {
            o.age += dt;
            o.life -= dt;
            if o.age > 0.4 && o.pos.distance(ppos) < 36.0 && !self.dead {
                taken = Some(o.up);
            }
        }
        if let Some(up) = taken {
            self.orbs.clear();
            self.apply_upgrade(up);
        }
        self.orbs.retain(|o| o.life > 0.0);
        if self.orbs.is_empty() && self.pending_orbs > 0 {
            self.pending_orbs -= 1;
            self.spawn_orbs(14.0);
        }
    }

    /// Wipe hostile things (used when the music stops or a song ends).
    pub fn calm_down(&mut self) {
        for b in self.bullets.iter_mut() {
            b.fade = b.fade.min(0.99);
        }
        self.lasers.clear();
        self.waves.clear();
        self.portals.clear();
        self.pulses.clear();
        for t in self.tiles.iter_mut() {
            t.armed = false;
        }
        let dead: Vec<(EnemyKind, Vec2)> = self.enemies.iter().map(|e| (e.kind, e.pos)).collect();
        self.enemies.clear();
        for (_, pos) in dead {
            self.burst(pos, 8, 300.0, C_SECONDARY, 4.0);
        }
    }
}
