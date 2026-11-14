//! The simulation: player, the Heart, bullets, enemies, tiles, lasers, shockwaves, pickups,
//! upgrades and scoring. No rendering here.

use macroquad::math::{vec2, Vec2};
use std::f32::consts::TAU;

use crate::analysis::{Feat, SectionKind};
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

const FIRE_DIVS: [f32; 5] = [2.0, 3.0, 4.0, 6.0, 8.0];

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
            hp: 5,
            max_hp: 5,
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

pub struct Bullet {
    pub pos: Vec2,
    pub vel: Vec2,
    pub r: f32,
    pub soft: bool,
    pub age: f32,
    pub fade: f32,
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
            tiles: vec![Tile { strike_t: -1.0, lead: 1.0, armed: false, flash: 0.0 }; COLS * ROWS],
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
        }
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
        self.orbs.clear();
        for t in self.tiles.iter_mut() {
            *t = Tile { strike_t: -1.0, lead: 1.0, armed: false, flash: 0.0 };
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
        match e.ev {
            Ev::Beat { down } => self.on_beat(down),
            Ev::Pulse { pattern, count, speed, rot, soft } => self.emit_pulse(pattern, count as usize, speed, rot, soft, late),
            Ev::Spawn { kind, x, y } => {
                if self.enemies.len() + self.portals.len() < MAX_ENEMIES {
                    self.portals.push(Portal { kind, pos: vec2(x, y), t0: e.at(), t1: e.t });
                }
            }
            Ev::Shards { x, y, n } => {
                for _ in 0..n {
                    let j = vec2(self.rng.range(-14.0, 14.0), self.rng.range(-14.0, 14.0));
                    self.shards.push(Shard { pos: vec2(x, y) + j, vel: Vec2::ZERO, life: 7.0, value: 1, heal: false, age: 0.0 });
                }
            }
            Ev::Laser { horizontal, from, to, dur } => {
                if self.lasers.len() < 6 {
                    self.lasers.push(Laser { horizontal, from, to, t0: e.t, dur, lead: e.lead.max(0.3) });
                }
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
                            }
                        }
                    }
                }
            }
            Ev::Shockwave { gap, gaps } => {
                self.waves.push(Wave { t0: e.t, lead: e.lead.max(0.3), gap, gaps, speed: 430.0 });
            }
            Ev::Section { idx, kind, intensity: _, palette } => {
                self.section_idx = idx;
                self.section_kind = kind;
                self.palette = palette;
                self.section_banner = 2.4;
                self.heart.reform = 1.0;
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
        let even = self.beat_count % 2 == 0;
        let ppos = self.player.pos;
        let speed = 210.0 + 90.0 * self.intensity;
        let mut new_bullets: Vec<Bullet> = Vec::new();
        for en in self.enemies.iter_mut() {
            match en.kind {
                EnemyKind::Orbiter if even => {
                    let d = (ppos - en.pos).normalize_or_zero();
                    new_bullets.push(Bullet { pos: en.pos + d * en.r, vel: d * speed, r: 7.0, soft: false, age: 0.0, fade: 1.0 });
                    en.flash = 0.6;
                }
                EnemyKind::Elite if even => {
                    let base = en.age;
                    for k in 0..8 {
                        let d = dir(base + k as f32 * TAU / 8.0);
                        new_bullets.push(Bullet { pos: en.pos + d * en.r, vel: d * speed * 0.85, r: 7.0, soft: k % 2 == 0, age: 0.0, fade: 1.0 });
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
        let dmg = 1.0 + 0.5 * self.player.up(UpgradeId::Gain) as f32;
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

    fn emit_pulse(&mut self, pattern: Pattern, mut count: usize, speed: f32, rot: f32, mut soft: bool, late: f32) {
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
        let push = |b: &mut Vec<Bullet>, a: f32, sp: f32| {
            let d = dir(a);
            let vel = d * sp;
            b.push(Bullet { pos: d * (HEART_R * 0.85) + vel * late, vel, r, soft, age: 0.0, fade: 1.0 });
        };
        let b = &mut self.bullets;
        match pattern {
            Pattern::Ring => {
                for k in 0..count {
                    push(b, rot + k as f32 * TAU / count as f32, speed);
                }
            }
            Pattern::Fan => {
                let base = self.player.pos.y.atan2(self.player.pos.x);
                let n = count.max(1) | 1;
                let spread = 0.16 * (n as f32 - 1.0);
                for k in 0..n {
                    let f = if n > 1 { k as f32 / (n - 1) as f32 - 0.5 } else { 0.0 };
                    push(b, base + f * spread, speed * 1.15);
                }
            }
            Pattern::Spiral => {
                let arms = 3usize;
                let per = (count / arms).max(1);
                for a in 0..arms {
                    for j in 0..per {
                        push(b, rot + a as f32 * TAU / arms as f32 + j as f32 * 0.11, speed * (1.0 - 0.11 * j as f32).max(0.4));
                    }
                }
            }
            Pattern::Cross => {
                let per = (count / 4).max(1);
                for a in 0..4 {
                    for j in 0..per {
                        push(b, rot + a as f32 * TAU / 4.0, speed * (1.0 - 0.12 * j as f32).max(0.35));
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
        let pos = self.player.pos;
        self.text(pos + vec2(0.0, -50.0), up.name(), C_ACCENT, 34.0, 1.8);
        self.text(pos + vec2(0.0, -22.0), up.desc(), C_WHITE, 18.0, 1.8);
        self.burst(pos, 26, 420.0, C_ACCENT, 5.0);
        self.flash = self.flash.max(0.15);
    }

    fn hurt_player(&mut self) {
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
            return;
        }
        p.hp -= 1;
        p.invuln = 1.3;
        let pos = p.pos;
        self.stats.hits += 1;
        self.combo *= 0.5;
        self.shake = self.shake.max(0.7);
        self.flash = self.flash.max(0.45);
        self.burst(pos, 22, 460.0, C_DANGER, 5.0);
        // Mercy: clear nearby bullets.
        for b in self.bullets.iter_mut() {
            if b.pos.distance(pos) < 150.0 {
                b.fade = b.fade.min(0.99);
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
                vel = d * 720.0;
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

        // ---- decays ----
        self.beat_pulse = (self.beat_pulse - dt * 4.5).max(0.0);
        self.bar_pulse = (self.bar_pulse - dt * 2.2).max(0.0);
        self.shake = (self.shake - dt * 2.4).max(0.0);
        self.flash = (self.flash - dt * 2.0).max(0.0);
        self.section_banner = (self.section_banner - dt).max(0.0);
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
        if input.dash && p.dash_cd <= 0.0 && p.dash_t <= 0.0 {
            p.dash_t = 0.16;
            p.dash_cd = p.dash_cd_max;
            p.dash_dir = if mv.length_squared() > 0.01 { mv.normalize() } else { p.aim_dir };
        }
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
            self.hurt_player();
        }

        // ---- beat-quantised gun ----
        let lv = self.player.up(UpgradeId::Tempo) as usize;
        let div = FIRE_DIVS[lv.min(FIRE_DIVS.len() - 1)];
        let idx = (beat_pos * div).floor() as i64;
        if idx != self.player.fire_phase {
            self.player.fire_phase = idx;
            if input.fire {
                self.fire();
            }
        }
    }

    fn fire(&mut self) {
        let p = &mut self.player;
        let n = 1 + p.ups[UpgradeId::Chord as usize] as usize;
        let dmg = 1.0 + 0.5 * p.ups[UpgradeId::Gain as usize] as f32;
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
        let slow = if active { 1.0 } else { 0.15 };
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
                        EnemyKind::Chaser => 125.0 + 75.0 * intensity,
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
            self.hurt_player();
        }
    }

    fn update_bullets(&mut self, dt: f32, active: bool) {
        let ppos = self.player.pos;
        let mut hit = false;
        let vulnerable = self.player.invuln <= 0.0 && self.player.dash_t <= 0.0 && !self.dead;
        for b in self.bullets.iter_mut() {
            b.pos += b.vel * dt;
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
                b.fade = -1.0;
            }
        }
        self.bullets.retain(|b| b.fade > 0.0);
        if hit {
            self.hurt_player();
        }
    }

    fn update_hazards(&mut self, dt: f32, t: f32, active: bool) {
        let ppos = self.player.pos;
        let mut hurt = false;

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
                        if active && ptile == Some((c, r)) {
                            hurt = true;
                        }
                    }
                }
            }
        }

        // Lasers
        for l in &self.lasers {
            if active && l.active(t) {
                let pos = l.pos_at(t);
                let d = if l.horizontal { ppos.y - pos } else { ppos.x - pos };
                if d.abs() < 13.0 + PLAYER_HIT_R {
                    hurt = true;
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
                    hurt = true;
                }
            }
        }
        let before = self.waves.len();
        self.waves.retain(|w| w.radius(t) < 980.0);
        let _ = before;
        if hurt {
            self.hurt_player();
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
