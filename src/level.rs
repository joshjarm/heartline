//! Level events and the generators that turn analysed audio into them.
//! `generate` plans a whole song; `LiveGen` does the same mapping incrementally for live audio.

use crate::analysis::{Analysis, Band, Onset, SectionKind};
use crate::rng::Rng;

pub const COLS: usize = 15;
pub const ROWS: usize = 9;
pub const TILE: f32 = 96.0;
pub const HALF_W: f32 = COLS as f32 * TILE * 0.5;
pub const HALF_H: f32 = ROWS as f32 * TILE * 0.5;
pub const N_PALETTES: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    Ring,
    Fan,
    Spiral,
    Cross,
    Flower,
    Arc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnemyKind {
    Chaser,
    Darter,
    Orbiter,
    Splitter,
    Mini,
    Elite,
}

#[derive(Clone, Copy, Debug)]
pub enum TilePattern {
    Checker(u8),
    Rows(u8, u8),
    Cols(u8, u8),
    Ring(u8),
    Cross(i8, i8),
    Diag(u8, u8),
    Scatter(u32, u8),
}

/// Whether tile (c, r) is part of a pattern.
pub fn tile_in_pattern(p: TilePattern, c: usize, r: usize) -> bool {
    let (cc, rc) = ((COLS / 2) as i32, (ROWS / 2) as i32);
    let (ci, ri) = (c as i32, r as i32);
    // The Heart's own tile is never struck.
    if ci == cc && ri == rc {
        return false;
    }
    match p {
        TilePattern::Checker(par) => (c + r) % 2 == par as usize % 2,
        TilePattern::Rows(m, ph) => r % (m.max(2) as usize) == ph as usize % m.max(2) as usize,
        TilePattern::Cols(m, ph) => c % (m.max(2) as usize) == ph as usize % m.max(2) as usize,
        TilePattern::Ring(d) => {
            // Stretch horizontally so it reads as a ring on the 15x9 grid.
            let dx = ((ci - cc).abs() as f32 / 1.6).round() as i32;
            let dy = (ri - rc).abs();
            dx.max(dy) == d as i32
        }
        TilePattern::Cross(x, y) => ci == cc + x as i32 || ri == rc + y as i32,
        TilePattern::Diag(m, ph) => (c + r) % (m.max(2) as usize) == ph as usize % m.max(2) as usize,
        TilePattern::Scatter(seed, count) => {
            let mut h = seed ^ ((c as u32).wrapping_mul(0x9E37_79B1)) ^ ((r as u32).wrapping_mul(0x85EB_CA6B));
            h ^= h >> 15;
            h = h.wrapping_mul(0x2C1B_3C6D);
            h ^= h >> 12;
            (h % (COLS * ROWS) as u32) < count as u32
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Ev {
    Beat { down: bool },
    Pulse { pattern: Pattern, count: u16, speed: f32, rot: f32, soft: bool },
    Spawn { kind: EnemyKind, x: f32, y: f32 },
    Shards { x: f32, y: f32, n: u8 },
    /// A beam spanning the arena. Horizontal beams sit at a y position and sweep in y.
    Laser { horizontal: bool, from: f32, to: f32, dur: f32 },
    Tiles { pattern: TilePattern },
    Shockwave { gap: f32, gaps: u8 },
    Section { idx: u16, kind: SectionKind, intensity: f32, palette: u8 },
    Orbs,
    Heal { x: f32, y: f32 },
    End,
}

#[derive(Clone, Copy, Debug)]
pub struct LevelEvent {
    /// Time the event "lands" on (the musical moment).
    pub t: f32,
    /// How long before `t` it should be telegraphed.
    pub lead: f32,
    pub ev: Ev,
}

impl LevelEvent {
    pub fn at(&self) -> f32 {
        self.t - self.lead
    }
}

pub struct Level {
    pub events: Vec<LevelEvent>,
    pub duration: f32,
    pub bpm: f32,
    pub beats: Vec<f32>,
    pub seed: u64,
    pub active_secs: f32,
}

#[derive(Clone, Copy)]
struct Deck {
    pulse_a: Pattern,
    pulse_b: Pattern,
    soft_every: u32,
    rot_step: f32,
    tiles: [TilePattern; 3],
}

fn make_deck(rng: &mut Rng, kind: SectionKind) -> Deck {
    let calm = [Pattern::Ring, Pattern::Fan, Pattern::Arc];
    let normal = [Pattern::Ring, Pattern::Fan, Pattern::Spiral, Pattern::Cross, Pattern::Arc];
    let hot = [Pattern::Ring, Pattern::Spiral, Pattern::Cross, Pattern::Flower, Pattern::Arc];
    let pool: &[Pattern] = match kind {
        SectionKind::Calm => &calm,
        SectionKind::Normal => &normal,
        _ => &hot,
    };
    let a = rng.pick(pool);
    let mut b = rng.pick(pool);
    if b == a {
        b = if a == Pattern::Ring { Pattern::Fan } else { Pattern::Ring };
    }
    let tile = |rng: &mut Rng| -> TilePattern {
        match rng.below(7) {
            0 => TilePattern::Checker(rng.below(2) as u8),
            1 => TilePattern::Rows(2 + rng.below(2) as u8, rng.below(3) as u8),
            2 => TilePattern::Cols(2 + rng.below(3) as u8, rng.below(4) as u8),
            3 => TilePattern::Ring(2 + rng.below(3) as u8),
            4 => TilePattern::Cross(rng.below(9) as i8 - 4, rng.below(5) as i8 - 2),
            5 => TilePattern::Diag(3 + rng.below(2) as u8, rng.below(4) as u8),
            _ => TilePattern::Scatter(rng.next_u64() as u32, 28 + rng.below(24) as u8),
        }
    };
    Deck {
        pulse_a: a,
        pulse_b: b,
        soft_every: 3 + rng.below(3) as u32,
        rot_step: rng.pick(&[0.0, 0.196, 0.262, 0.393, 2.399]) * rng.sign(),
        tiles: [tile(rng), tile(rng), tile(rng)],
    }
}

fn shift_tiles(p: TilePattern, n: u32) -> TilePattern {
    match p {
        TilePattern::Checker(a) => TilePattern::Checker(((a as u32 + n) % 2) as u8),
        TilePattern::Rows(m, ph) => TilePattern::Rows(m, ((ph as u32 + n) % m.max(2) as u32) as u8),
        TilePattern::Cols(m, ph) => TilePattern::Cols(m, ((ph as u32 + n) % m.max(2) as u32) as u8),
        TilePattern::Ring(d) => TilePattern::Ring(2 + ((d as u32 - 2 + n) % 3) as u8),
        TilePattern::Cross(x, y) => {
            let nx = ((x as i32 + 4 + 3 * n as i32).rem_euclid(9) - 4) as i8;
            let ny = ((y as i32 + 2 + n as i32).rem_euclid(5) - 2) as i8;
            TilePattern::Cross(nx, ny)
        }
        TilePattern::Diag(m, ph) => TilePattern::Diag(m, ((ph as u32 + n) % m.max(2) as u32) as u8),
        TilePattern::Scatter(s, c) => TilePattern::Scatter(s.wrapping_add(n.wrapping_mul(7919)), c),
    }
}

fn pulse_event(
    rng: &mut Rng,
    pattern: Pattern,
    intensity: f32,
    strength: f32,
    kind: SectionKind,
    diff: f32,
    rot: f32,
    soft: bool,
    t: f32,
) -> LevelEvent {
    let i = intensity;
    let s = 0.6 + 0.6 * strength;
    let calm = if kind == SectionKind::Calm { 0.6 } else { 1.0 };
    let count = match pattern {
        Pattern::Ring => (9.0 + 14.0 * i) * s,
        Pattern::Fan => 3.0 + 4.0 * i * s,
        Pattern::Spiral => (4.0 + 6.0 * i) * s,
        Pattern::Cross => 8.0 + 12.0 * i * s,
        Pattern::Flower => (14.0 + 16.0 * i) * s,
        Pattern::Arc => (10.0 + 14.0 * i) * s,
    } * diff
        * calm;
    let speed = (165.0 + 150.0 * i + 40.0 * strength) * (0.85 + 0.15 * diff) * if kind == SectionKind::Calm { 0.8 } else { 1.0 };
    let jitter = rng.range(-0.02, 0.02);
    LevelEvent {
        t,
        lead: 0.0,
        ev: Ev::Pulse { pattern, count: count.round().clamp(3.0, 64.0) as u16, speed, rot: rot + jitter, soft },
    }
}

/// Target hostile bullets per second for a given intensity.
fn bullet_rate(intensity: f32, kind: SectionKind, diff: f32) -> f32 {
    (10.0 + 32.0 * intensity) * diff * if kind == SectionKind::Calm { 0.6 } else { 1.0 }
}

/// Token bucket that keeps bullet density bounded no matter how busy the drums are.
/// Pulses that cannot be afforded still fire with `count = 0` so the Heart visibly throbs.
struct Budget {
    tokens: f32,
    last_t: f32,
}

impl Budget {
    fn new() -> Self {
        Budget { tokens: 20.0, last_t: 0.0 }
    }
    fn spend(&mut self, e: &mut LevelEvent, rate: f32) {
        let dt = (e.t - self.last_t).clamp(0.0, 4.0);
        self.last_t = e.t;
        self.tokens = (self.tokens + dt * rate).min(rate * 1.6);
        if let Ev::Pulse { count, .. } = &mut e.ev {
            let want = *count as f32;
            let give = if self.tokens >= want {
                want
            } else if self.tokens >= 3.0 {
                self.tokens.floor()
            } else {
                0.0
            };
            self.tokens -= give;
            *count = give as u16;
        }
    }
}

fn edge_point(rng: &mut Rng) -> (f32, f32) {
    let inset = 34.0;
    match rng.below(4) {
        0 => (rng.range(-HALF_W + inset, HALF_W - inset), -HALF_H + inset),
        1 => (rng.range(-HALF_W + inset, HALF_W - inset), HALF_H - inset),
        2 => (-HALF_W + inset, rng.range(-HALF_H + inset, HALF_H - inset)),
        _ => (HALF_W - inset, rng.range(-HALF_H + inset, HALF_H - inset)),
    }
}

fn pick_enemy(rng: &mut Rng, kind: SectionKind) -> EnemyKind {
    let r = rng.f32();
    match kind {
        SectionKind::Calm => EnemyKind::Chaser,
        SectionKind::Normal => {
            if r < 0.68 {
                EnemyKind::Chaser
            } else if r < 0.88 {
                EnemyKind::Darter
            } else {
                EnemyKind::Orbiter
            }
        }
        _ => {
            if r < 0.45 {
                EnemyKind::Chaser
            } else if r < 0.68 {
                EnemyKind::Darter
            } else if r < 0.88 {
                EnemyKind::Splitter
            } else {
                EnemyKind::Orbiter
            }
        }
    }
}

fn spawn_event(rng: &mut Rng, kind: EnemyKind, t: f32, lead: f32) -> LevelEvent {
    let (x, y) = if kind == EnemyKind::Orbiter {
        let a = rng.angle();
        (a.cos() * 330.0, a.sin() * 300.0)
    } else {
        edge_point(rng)
    };
    LevelEvent { t, lead, ev: Ev::Spawn { kind, x, y } }
}

fn laser_event(rng: &mut Rng, t: f32, dur: f32, lead: f32) -> LevelEvent {
    let horizontal = rng.chance(0.5);
    let half = if horizontal { HALF_H } else { HALF_W };
    let from = rng.range(-half * 0.92, half * 0.92);
    let travel = dur * rng.range(90.0, 210.0) * rng.sign();
    let to = (from + travel).clamp(-half * 0.96, half * 0.96);
    LevelEvent { t, lead, ev: Ev::Laser { horizontal, from, to, dur } }
}

fn shard_point(ang: f32, wobble: f32) -> (f32, f32) {
    let r = 250.0 + 110.0 * wobble.sin();
    (ang.cos() * r * 1.55, ang.sin() * r * 0.9)
}

// ---------------------------------------------------------------------------------------------
// Whole-song generation
// ---------------------------------------------------------------------------------------------

pub fn generate(an: &Analysis, user_seed: u64, diff: f32) -> Level {
    let seed = an.seed ^ user_seed.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    let mut rng = Rng::new(seed);
    let mut ev: Vec<LevelEvent> = Vec::with_capacity(an.onsets.len() + an.beats.len() + 64);
    let beat_len = if an.bpm > 1.0 { 60.0 / an.bpm } else { 0.5 };

    let t_start = an
        .silences
        .first()
        .filter(|s| s.0 <= 0.01)
        .map(|s| s.1)
        .unwrap_or(0.0);
    let grace = t_start + 2.2;

    for (i, &t) in an.beats.iter().enumerate() {
        let down = i >= an.downbeat && (i - an.downbeat) % 4 == 0;
        ev.push(LevelEvent { t, lead: 0.0, ev: Ev::Beat { down } });
    }

    // ---- sections ----
    let mut palette = rng.below(N_PALETTES as usize) as u8;
    let mut decks: Vec<Deck> = Vec::with_capacity(an.sections.len());
    let mut last_orbs = t_start;
    for (idx, s) in an.sections.iter().enumerate() {
        if idx > 0 {
            palette = (palette + 1 + rng.below(N_PALETTES as usize - 1) as u8) % N_PALETTES;
        }
        decks.push(make_deck(&mut rng, s.kind));
        ev.push(LevelEvent {
            t: s.t0,
            lead: 0.0,
            ev: Ev::Section { idx: idx as u16, kind: s.kind, intensity: s.intensity, palette },
        });
        if idx == 0 || s.t0 < grace {
            continue;
        }
        match s.kind {
            SectionKind::Drop => {
                ev.push(LevelEvent {
                    t: s.t0,
                    lead: 1.1,
                    ev: Ev::Shockwave { gap: rng.angle(), gaps: if diff < 0.8 { 3 } else { 2 } },
                });
                ev.push(spawn_event(&mut rng, EnemyKind::Elite, s.t0 + 2.0 * beat_len, 0.8));
            }
            SectionKind::Calm => {
                ev.push(LevelEvent { t: s.t0 + 0.8, lead: 0.0, ev: Ev::Orbs });
                last_orbs = s.t0;
                if rng.chance(0.6) {
                    let a = rng.angle();
                    ev.push(LevelEvent { t: s.t0 + 1.5, lead: 0.0, ev: Ev::Heal { x: a.cos() * 420.0, y: a.sin() * 260.0 } });
                }
            }
            _ => {}
        }
        if s.t0 - last_orbs > 55.0 {
            ev.push(LevelEvent { t: s.t0 + 0.8, lead: 0.0, ev: Ev::Orbs });
            last_orbs = s.t0;
        }
    }
    let section_index = |t: f32| -> usize {
        an.sections
            .iter()
            .position(|s| t >= s.t0 && t < s.t1)
            .unwrap_or(an.sections.len().saturating_sub(1))
    };

    // ---- onsets ----
    let mut last_pulse = -10.0f32;
    let mut last_spawn = -10.0f32;
    let mut pulse_n = vec![0u32; an.sections.len().max(1)];
    let mut sec_rot: Vec<f32> = (0..an.sections.len().max(1)).map(|_| rng.angle()).collect();
    let mut shard_ang = rng.angle();
    let shard_dir = rng.sign();
    let bars: Vec<f32> = an.beats.iter().skip(an.downbeat).step_by(4).copied().collect();
    let near_bar = |t: f32| -> bool { bars.iter().any(|&b| (b - t).abs() < 0.14 * beat_len.max(0.3)) };
    let mut low_seen = vec![0u32; an.sections.len().max(1)];
    let mut budget = Budget::new();

    for o in &an.onsets {
        if o.t < grace || an.in_silence(o.t) {
            continue;
        }
        let si = section_index(o.t);
        let sec = an.sections[si];
        let deck = decks[si];
        let i = an.intensity_at(o.t);
        match o.band {
            Band::Low => {
                low_seen[si] += 1;
                let gap = match sec.kind {
                    SectionKind::Calm => 0.9,
                    SectionKind::Normal => 0.46,
                    _ => 0.23,
                } * beat_len;
                if o.t - last_pulse < gap.max(0.13) {
                    continue;
                }
                last_pulse = o.t;
                let n = pulse_n[si];
                pulse_n[si] += 1;
                let accent = near_bar(o.t);
                let pattern = if accent { deck.pulse_b } else { deck.pulse_a };
                sec_rot[si] += deck.rot_step;
                let soft = n % deck.soft_every == deck.soft_every - 1;
                let mut pe = pulse_event(&mut rng, pattern, i, o.strength, sec.kind, diff, sec_rot[si], soft, o.t);
                budget.spend(&mut pe, bullet_rate(i, sec.kind, diff));
                ev.push(pe);
            }
            Band::Mid => {
                let mut p = (0.12 + 0.32 * i) * diff;
                if sec.kind == SectionKind::Calm {
                    p *= 0.4;
                }
                let hot = matches!(sec.kind, SectionKind::Intense | SectionKind::Drop);
                if o.t - last_spawn < if hot { 0.3 } else { 0.42 } || !rng.chance(p.min(0.9)) {
                    continue;
                }
                last_spawn = o.t;
                let kind = pick_enemy(&mut rng, sec.kind);
                ev.push(spawn_event(&mut rng, kind, o.t, beat_len.clamp(0.4, 0.8)));
            }
            Band::High => match sec.kind {
                SectionKind::Calm | SectionKind::Normal => {
                    if rng.chance(0.3) {
                        shard_ang += shard_dir * rng.range(0.25, 0.6);
                        let (x, y) = shard_point(shard_ang, o.t * 0.7);
                        ev.push(LevelEvent { t: o.t, lead: 0.3, ev: Ev::Shards { x, y, n: 1 + (o.strength > 0.6) as u8 } });
                    }
                }
                _ => {
                    if rng.chance(0.07 * diff) && o.t - last_spawn > 0.3 {
                        last_spawn = o.t;
                        ev.push(spawn_event(&mut rng, EnemyKind::Darter, o.t, beat_len.clamp(0.4, 0.8)));
                    } else if rng.chance(0.12) {
                        shard_ang += shard_dir * rng.range(0.25, 0.6);
                        let (x, y) = shard_point(shard_ang, o.t * 0.7);
                        ev.push(LevelEvent { t: o.t, lead: 0.3, ev: Ev::Shards { x, y, n: 1 } });
                    }
                }
            },
        }
    }

    // Sections with no detected kicks still need a heartbeat: pulse on the beat grid instead.
    for (si, sec) in an.sections.iter().enumerate() {
        let len = sec.t1 - sec.t0;
        if len < 4.0 || (low_seen[si] as f32) > len / (beat_len * 4.0) {
            continue;
        }
        let deck = decks[si];
        let every = if sec.kind == SectionKind::Calm { 4 } else { 2 };
        let mut grid_budget = Budget::new();
        for (bi, &bt) in an.beats.iter().enumerate() {
            if bt < sec.t0 || bt >= sec.t1 || bt < grace || an.in_silence(bt) || bi % every != 0 {
                continue;
            }
            sec_rot[si] += deck.rot_step;
            let n = pulse_n[si];
            pulse_n[si] += 1;
            let soft = n % deck.soft_every == deck.soft_every - 1;
            let pat = if bi % 4 == 0 { deck.pulse_b } else { deck.pulse_a };
            let ib = an.intensity_at(bt);
            let mut pe = pulse_event(&mut rng, pat, ib, 0.5, sec.kind, diff, sec_rot[si], soft, bt);
            grid_budget.spend(&mut pe, bullet_rate(ib, sec.kind, diff));
            ev.push(pe);
        }
    }

    // ---- lasers from sustained phrases ----
    let mut last_laser = -10.0f32;
    for &(a, b) in &an.phrases {
        if a < grace + 1.0 || an.in_silence(a) {
            continue;
        }
        let sec = an.sections[section_index(a)];
        let min_gap = match sec.kind {
            SectionKind::Calm => continue,
            SectionKind::Normal => 3.2,
            SectionKind::Intense => 1.8,
            SectionKind::Drop => 1.2,
        } / diff.max(0.5);
        if a - last_laser < min_gap {
            continue;
        }
        last_laser = a;
        let dur = (b - a).clamp(0.7, 3.2);
        ev.push(laser_event(&mut rng, a, dur, 0.9));
    }

    // ---- tile strikes on bars ----
    let mut tile_n = 0u32;
    for (j, &bt) in bars.iter().enumerate() {
        if bt < grace + 2.0 || an.in_silence(bt) {
            continue;
        }
        let si = section_index(bt);
        let sec = an.sections[si];
        let hard = diff > 1.2;
        let every = match sec.kind {
            SectionKind::Calm => continue,
            SectionKind::Normal => {
                if hard {
                    2
                } else {
                    4
                }
            }
            _ => {
                if hard {
                    1
                } else {
                    2
                }
            }
        };
        if j % every != 0 {
            continue;
        }
        let deck = decks[si];
        let pat = shift_tiles(deck.tiles[(tile_n % 3) as usize], tile_n / 3);
        tile_n += 1;
        let lead = (2.0 * beat_len).clamp(0.75, 1.3);
        ev.push(LevelEvent { t: bt, lead, ev: Ev::Tiles { pattern: pat } });
        if sec.kind == SectionKind::Drop && hard {
            let pat2 = shift_tiles(deck.tiles[((tile_n + 1) % 3) as usize], tile_n);
            ev.push(LevelEvent { t: bt + 2.0 * beat_len, lead, ev: Ev::Tiles { pattern: pat2 } });
        }
    }

    ev.push(LevelEvent { t: an.duration, lead: 0.0, ev: Ev::End });
    ev.sort_by(|a, b| a.at().partial_cmp(&b.at()).unwrap());

    let silent: f32 = an.silences.iter().map(|s| s.1 - s.0).sum();
    Level {
        events: ev,
        duration: an.duration,
        bpm: an.bpm,
        beats: an.beats.clone(),
        seed,
        active_secs: (an.duration - silent).max(1.0),
    }
}

// ---------------------------------------------------------------------------------------------
// Live generation
// ---------------------------------------------------------------------------------------------

pub struct LiveGen {
    rng: Rng,
    diff: f32,
    /// How far ahead of the audible clock events are known (≈ delay in delayed mode, 0 in reactive).
    pub lookahead: f32,
    deck: Deck,
    kind: SectionKind,
    palette: u8,
    section_idx: u16,
    section_t: f32,
    section_i: f32,
    last_pulse: f32,
    last_spawn: f32,
    last_laser: f32,
    last_orbs: f32,
    pulse_n: u32,
    low_n: u32,
    tile_n: u32,
    rot: f32,
    shard_ang: f32,
    pub intensity: f32,
    rate: f32,
    last_feat_t: f32,
    pending_section: bool,
    budget: Budget,
}

impl LiveGen {
    pub fn new(seed: u64, diff: f32, lookahead: f32) -> Self {
        let mut rng = Rng::new(seed);
        let deck = make_deck(&mut rng, SectionKind::Normal);
        let palette = rng.below(N_PALETTES as usize) as u8;
        LiveGen {
            rng,
            diff,
            lookahead,
            deck,
            kind: SectionKind::Normal,
            palette,
            section_idx: 0,
            section_t: 0.0,
            section_i: 0.5,
            last_pulse: -10.0,
            last_spawn: -10.0,
            last_laser: 0.0,
            last_orbs: 0.0,
            pulse_n: 0,
            low_n: 0,
            tile_n: 0,
            rot: 0.0,
            shard_ang: 0.0,
            intensity: 0.4,
            rate: 0.0,
            last_feat_t: 0.0,
            pending_section: true,
            budget: Budget::new(),
        }
    }

    fn sched(&self, t: f32, telegraph: f32) -> (f32, f32) {
        if self.lookahead >= telegraph {
            (t, telegraph)
        } else {
            (t + telegraph - self.lookahead, telegraph)
        }
    }

    /// Call when audio resumes after silence: forces a fresh section.
    pub fn resume(&mut self) {
        self.pending_section = true;
    }

    /// Feed one analysed frame (loudness drives the rolling intensity / section logic).
    pub fn feed(&mut self, t: f32, rms: f32, out: &mut Vec<LevelEvent>) {
        let dt = (t - self.last_feat_t).clamp(0.0, 0.2);
        self.last_feat_t = t;
        let k = 1.0 - (-dt / 2.5).exp();
        let target = (rms.powf(1.2) * 0.75 + (self.rate / 9.0).min(1.0) * 0.35).clamp(0.0, 1.0);
        self.intensity += (target - self.intensity) * k;
        self.rate *= (-dt / 2.0).exp();

        let change = self.intensity - self.section_i;
        if self.pending_section || (t - self.section_t > 12.0 && change.abs() > 0.2) || t - self.section_t > 45.0 {
            let first = self.pending_section && self.section_idx == 0;
            self.pending_section = false;
            self.kind = if self.intensity < 0.34 {
                SectionKind::Calm
            } else if self.intensity > 0.62 {
                if change > 0.22 && !first { SectionKind::Drop } else { SectionKind::Intense }
            } else {
                SectionKind::Normal
            };
            self.section_idx += 1;
            self.section_t = t;
            self.section_i = self.intensity;
            self.deck = make_deck(&mut self.rng, self.kind);
            self.palette = (self.palette + 1 + self.rng.below(N_PALETTES as usize - 1) as u8) % N_PALETTES;
            self.pulse_n = 0;
            out.push(LevelEvent {
                t,
                lead: 0.0,
                ev: Ev::Section { idx: self.section_idx, kind: self.kind, intensity: self.intensity, palette: self.palette },
            });
            if self.kind == SectionKind::Drop {
                let (te, lead) = self.sched(t, 1.1);
                out.push(LevelEvent { t: te, lead, ev: Ev::Shockwave { gap: self.rng.angle(), gaps: 2 } });
                let (te, lead) = self.sched(t + 1.0, 0.8);
                out.push(spawn_event(&mut self.rng, EnemyKind::Elite, te, lead));
            }
            if (self.kind == SectionKind::Calm && t - self.last_orbs > 25.0) || t - self.last_orbs > 75.0 {
                self.last_orbs = t;
                out.push(LevelEvent { t: t + 0.8, lead: 0.0, ev: Ev::Orbs });
            }
        }
    }

    pub fn on_onset(&mut self, o: &Onset, out: &mut Vec<LevelEvent>) {
        self.rate += 0.5;
        let i = self.intensity;
        match o.band {
            Band::Low => {
                self.low_n += 1;
                let gap = match self.kind {
                    SectionKind::Calm => 0.42,
                    SectionKind::Normal => 0.22,
                    _ => 0.13,
                };
                if o.t - self.last_pulse >= gap {
                    self.last_pulse = o.t;
                    self.rot += self.deck.rot_step;
                    let n = self.pulse_n;
                    self.pulse_n += 1;
                    let pattern = if n % 4 == 0 { self.deck.pulse_b } else { self.deck.pulse_a };
                    let soft = n % self.deck.soft_every == self.deck.soft_every - 1;
                    let mut pe = pulse_event(&mut self.rng, pattern, i, o.strength, self.kind, self.diff, self.rot, soft, o.t);
                    self.budget.spend(&mut pe, bullet_rate(i, self.kind, self.diff));
                    out.push(pe);
                }
                let every = match self.kind {
                    SectionKind::Calm => 0,
                    SectionKind::Normal => 24,
                    _ => 12,
                };
                if every > 0 && self.low_n % every == 0 {
                    let pat = shift_tiles(self.deck.tiles[(self.tile_n % 3) as usize], self.tile_n / 3);
                    self.tile_n += 1;
                    let (t, lead) = self.sched(o.t, 0.95);
                    out.push(LevelEvent { t, lead, ev: Ev::Tiles { pattern: pat } });
                }
                let laser_gap = match self.kind {
                    SectionKind::Calm => f32::MAX,
                    SectionKind::Normal => 7.0,
                    _ => 3.5,
                } / self.diff.max(0.5);
                if o.t - self.last_laser > laser_gap && o.strength > 0.5 {
                    self.last_laser = o.t;
                    let (t, lead) = self.sched(o.t, 0.9);
                    let dur = self.rng.range(1.0, 2.4);
                    out.push(laser_event(&mut self.rng, t, dur, lead));
                }
            }
            Band::Mid => {
                let mut p = (0.12 + 0.32 * i) * self.diff;
                if self.kind == SectionKind::Calm {
                    p *= 0.4;
                }
                if o.t - self.last_spawn < 0.35 || !self.rng.chance(p.min(0.9)) {
                    return;
                }
                self.last_spawn = o.t;
                let kind = pick_enemy(&mut self.rng, self.kind);
                let (t, lead) = self.sched(o.t, 0.55);
                out.push(spawn_event(&mut self.rng, kind, t, lead));
            }
            Band::High => {
                let hot = matches!(self.kind, SectionKind::Intense | SectionKind::Drop);
                if hot && self.rng.chance(0.07 * self.diff) && o.t - self.last_spawn > 0.3 {
                    self.last_spawn = o.t;
                    let (t, lead) = self.sched(o.t, 0.55);
                    out.push(spawn_event(&mut self.rng, EnemyKind::Darter, t, lead));
                } else if self.rng.chance(if hot { 0.12 } else { 0.3 }) {
                    self.shard_ang += self.rng.range(0.25, 0.6);
                    let (x, y) = shard_point(self.shard_ang, o.t * 0.7);
                    let (t, lead) = self.sched(o.t, 0.3);
                    out.push(LevelEvent { t, lead, ev: Ev::Shards { x, y, n: 1 } });
                }
            }
        }
    }
}
