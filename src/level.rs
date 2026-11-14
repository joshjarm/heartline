//! Level events and the generators that turn analysed audio into them.
//! `generate` plans a whole song; `LiveGen` does the same mapping incrementally for live audio.
//!
//! Design rules (0.2):
//!  * **The song decides when and in what shape, never how hard.** Every budget below is per
//!    second, so a slow ballad and a fast track put the same pressure on the player; tempo and
//!    drums only choose the moments attacks land on and which patterns are used.
//!  * **Attacks are directed.** Patterns are cones, walls with a gap, pincers and lines aimed
//!    at the player (announced before they fire), so there is always obvious empty space to
//!    move into. Omnidirectional sprays are reserved for Hard.
//!  * **Bars have a shape.** Each section kind owns a motif: the attack that lands on beat 1,
//!    2, 3, 4 (and the off-beats) is the same every bar, and the same again when that kind of
//!    section returns, so patterns can be learned.

use crate::analysis::{Analysis, Band, Onset, SectionKind};
use crate::rng::Rng;

pub const COLS: usize = 15;
pub const ROWS: usize = 9;
pub const TILE: f32 = 96.0;
pub const HALF_W: f32 = COLS as f32 * TILE * 0.5;
pub const HALF_H: f32 = ROWS as f32 * TILE * 0.5;
pub const N_PALETTES: u8 = 5;

// ---------------------------------------------------------------------------------------------
// Tuning (difficulty 1.0 = Normal). All rates are per second of music.
// ---------------------------------------------------------------------------------------------

/// Hostile bullets per second the Heart may fire at average song energy.
const BULLETS_PER_SEC: f32 = 10.0;
/// Enemy "points" per second (a Chaser costs 1).
const ENEMY_POINTS_PER_SEC: f32 = 0.20;
/// Target seconds between Heart pulses per section kind (Calm, Normal, Intense, Drop).
const PULSE_INTERVAL: [f32; 4] = [1.6, 1.0, 0.72, 0.66];
/// Target seconds between tile-strike phrases (0 = never).
const TILE_INTERVAL: [f32; 4] = [0.0, 9.0, 6.5, 5.5];
/// Target seconds between lasers (0 = never).
const LASER_INTERVAL: [f32; 4] = [0.0, 11.0, 7.5, 6.5];
/// Nothing attacks during the first seconds of audio.
const GRACE: f32 = 3.0;
/// Speed of hi-hat darts.
const TICK_SPEED: f32 = 370.0;

pub const WALL_HALF: f32 = 1.25;
pub const WALL_GAP_HALF: f32 = 0.27;
pub const PINCER_OFFSET: f32 = 0.62;
pub const PINCER_SPREAD: f32 = 0.30;
pub const FAN_STEP: f32 = 0.2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// Full ring (omnidirectional). `rot` = absolute phase.
    Ring,
    /// Cone aimed at the player: bullets `FAN_STEP` apart.
    Fan,
    /// Two curved arms starting at `rot` (absolute).
    Spiral,
    /// Four lines of bullets; one arm is aimed at the player.
    Cross,
    /// Ring with petal-shaped speed (omnidirectional). `rot` = absolute phase.
    Flower,
    /// Three-quarter ring whose gap is centred on `rot` (absolute).
    Arc,
    /// Dense arc `2 * WALL_HALF` wide centred on the player, with one gap at `aim + rot`
    /// (`rot` is the signed offset of the gap, `WALL_GAP_HALF` wide each side).
    Wall,
    /// A fast line of bullets straight at the player.
    Burst,
    /// Two cones at `aim +- PINCER_OFFSET`, leaving the lane through the player open.
    Pincer,
}

impl Pattern {
    /// Whether the pattern is laid out relative to the player's direction.
    pub fn aimed(self) -> bool {
        matches!(self, Pattern::Fan | Pattern::Cross | Pattern::Wall | Pattern::Burst | Pattern::Pincer)
    }
    /// (min, max) bullets for one pulse at Normal.
    fn range(self) -> (f32, f32) {
        match self {
            Pattern::Ring => (8.0, 16.0),
            Pattern::Fan => (3.0, 5.0),
            Pattern::Spiral => (6.0, 12.0),
            Pattern::Cross => (8.0, 16.0),
            Pattern::Flower => (10.0, 18.0),
            Pattern::Arc => (9.0, 18.0),
            Pattern::Wall => (9.0, 16.0),
            Pattern::Burst => (3.0, 5.0),
            Pattern::Pincer => (6.0, 12.0),
        }
    }
    fn speed_mul(self) -> f32 {
        match self {
            Pattern::Burst => 1.3,
            Pattern::Wall | Pattern::Arc => 0.9,
            _ => 1.0,
        }
    }
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

impl EnemyKind {
    fn cost(self) -> f32 {
        match self {
            EnemyKind::Chaser => 1.0,
            EnemyKind::Darter => 0.7,
            EnemyKind::Orbiter => 1.8,
            EnemyKind::Splitter => 2.2,
            EnemyKind::Mini => 0.4,
            EnemyKind::Elite => 0.0,
        }
    }
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
    /// Columns `start .. start + width`.
    ColBand(u8, u8),
    /// Rows `start .. start + width`.
    RowBand(u8, u8),
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
        TilePattern::ColBand(s, w) => c >= s as usize && c < s as usize + w as usize,
        TilePattern::RowBand(s, w) => r >= s as usize && r < s as usize + w as usize,
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Ev {
    Beat { down: bool },
    /// A drum hit was heard (band 0 = kick, 1 = snare, 2 = hat). Purely cosmetic: drives the
    /// `World::hit` envelopes so visuals can react to every onset, not just the ones that attack.
    Hit { band: u8, strength: f32 },
    /// A Heart bullet pattern. With `lead > 0` it is announced early (approach ring) and fires on `t`.
    Pulse { pattern: Pattern, count: u16, speed: f32, rot: f32, soft: bool },
    /// A single fast dart aimed at the player (hi-hat streams).
    Tick { speed: f32 },
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

/// Whole-song overview for the HUD: the "heartline" of the track.
pub struct Timeline {
    /// Intensity 0..1 sampled evenly over the song (`TIMELINE_POINTS` values).
    pub curve: Vec<f32>,
    /// Section starts as a 0..1 fraction of the song, with their kind.
    pub marks: Vec<(f32, SectionKind)>,
}

pub const TIMELINE_POINTS: usize = 240;

impl Timeline {
    pub fn from_analysis(an: &Analysis) -> Timeline {
        let dur = an.duration.max(0.1);
        let n = an.intensity.len();
        let mut curve = Vec::with_capacity(TIMELINE_POINTS);
        for k in 0..TIMELINE_POINTS {
            if n == 0 {
                curve.push(0.0);
                continue;
            }
            let a = k * n / TIMELINE_POINTS;
            let z = ((k + 1) * n / TIMELINE_POINTS).clamp(a + 1, n);
            curve.push(an.intensity[a..z].iter().sum::<f32>() / (z - a) as f32);
        }
        let marks = an.sections.iter().map(|s| ((s.t0 / dur).clamp(0.0, 1.0), s.kind)).collect();
        Timeline { curve, marks }
    }
}

/// Continuous beat index at time `t` (extrapolates before the first and after the last beat).
pub fn beat_pos(beats: &[f32], t: f32) -> f32 {
    let n = beats.len();
    if n < 2 {
        return t * 2.0;
    }
    let i = beats.partition_point(|&b| b <= t);
    if i == 0 {
        return (t - beats[0]) / (beats[1] - beats[0]).max(0.05);
    }
    if i >= n {
        return (n - 1) as f32 + (t - beats[n - 1]) / (beats[n - 1] - beats[n - 2]).max(0.05);
    }
    (i - 1) as f32 + (t - beats[i - 1]) / (beats[i] - beats[i - 1]).max(0.05)
}

fn kind_idx(k: SectionKind) -> usize {
    match k {
        SectionKind::Calm => 0,
        SectionKind::Normal => 1,
        SectionKind::Intense => 2,
        SectionKind::Drop => 3,
    }
}

/// Budget multiplier from song energy `i` (0..1, song-relative, about 0.5 on average).
fn energy(i: f32) -> f32 {
    0.6 + 0.8 * i.clamp(0.0, 1.0)
}

/// Maps a song's raw intensity curve to a song-relative 0..1 scale (its own median becomes 0.5),
/// so quiet songs and loud songs fill the same budgets.
#[derive(Clone, Copy)]
struct Norm {
    p10: f32,
    p50: f32,
    p90: f32,
}

impl Norm {
    fn new(an: &Analysis) -> Norm {
        let mut v: Vec<f32> = Vec::with_capacity(an.intensity.len() / 8 + 1);
        for (i, &x) in an.intensity.iter().enumerate().step_by(8) {
            if !an.in_silence(i as f32 * an.hop_dt) {
                v.push(x);
            }
        }
        if v.len() < 8 {
            return Norm { p10: 0.1, p50: 0.5, p90: 0.9 };
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let q = |p: f32| v[((v.len() - 1) as f32 * p) as usize];
        let (p10, p50, p90) = (q(0.10), q(0.50), q(0.90));
        Norm { p10, p50: p50.max(p10 + 0.02), p90: p90.max(p50.max(p10 + 0.02) + 0.02) }
    }
    fn at(&self, raw: f32) -> f32 {
        let v = if raw <= self.p50 {
            0.15 + 0.35 * (raw - self.p10) / (self.p50 - self.p10)
        } else {
            0.5 + 0.35 * (raw - self.p50) / (self.p90 - self.p50)
        };
        v.clamp(0.0, 1.0)
    }
}

/// A sequence of tile strikes that plays out over consecutive beats.
#[derive(Clone, Copy, Debug)]
enum TileSeq {
    Single(TilePattern),
    /// A wall of 3 columns walking across the arena (true = left to right).
    SweepCols(bool),
    /// A wall of 2 rows walking across the arena (true = top to bottom).
    SweepRows(bool),
    /// Rings around the Heart, expanding (true) or closing in (false).
    Ripple(bool),
    /// One checker half, then the other.
    Flip(u8),
}

impl TileSeq {
    /// The strikes of this phrase as (step index, pattern). `n` varies repeats.
    fn steps(self, n: u32, out: &mut Vec<(u32, TilePattern)>) {
        out.clear();
        match self {
            TileSeq::Single(p) => out.push((0, shift_tiles(p, n))),
            TileSeq::SweepCols(ltr) => {
                let ltr = ltr ^ (n % 2 == 1);
                for k in 0..5u32 {
                    let band = if ltr { k } else { 4 - k };
                    out.push((k, TilePattern::ColBand((band * 3) as u8, 3)));
                }
            }
            TileSeq::SweepRows(ttb) => {
                let ttb = ttb ^ (n % 2 == 1);
                for k in 0..5u32 {
                    let band = if ttb { k } else { 4 - k };
                    out.push((k, TilePattern::RowBand((band * 2) as u8, 2)));
                }
            }
            TileSeq::Ripple(outward) => {
                let outward = outward ^ (n % 2 == 1);
                for k in 0..4u32 {
                    let d = if outward { k + 1 } else { 4 - k };
                    out.push((k, TilePattern::Ring(d as u8)));
                }
            }
            TileSeq::Flip(par) => {
                let par = (par as u32 + n) % 2;
                out.push((0, TilePattern::Checker(par as u8)));
                out.push((2, TilePattern::Checker(1 - par as u8)));
            }
        }
    }
}

/// The attack vocabulary of one section kind.
#[derive(Clone, Copy)]
struct Deck {
    /// Pulse pattern per half-beat position in the bar (index 0 = the downbeat).
    moves: [Pattern; 8],
    /// How far the accent's gap / the spiral's phase advances each time it is used.
    rot_step: f32,
    /// Side of the first wall gap (+1 / -1); alternates every use.
    gap_side: f32,
    /// Every n-th non-accent pulse is made of shootable notes.
    soft_every: u32,
    tiles: [TileSeq; 3],
}

fn single_tiles(rng: &mut Rng) -> TilePattern {
    match rng.below(6) {
        0 => TilePattern::Rows(3, rng.below(3) as u8),
        1 => TilePattern::Cols(3 + rng.below(2) as u8, rng.below(4) as u8),
        2 => TilePattern::Ring(2 + rng.below(3) as u8),
        3 => TilePattern::Cross(rng.below(9) as i8 - 4, rng.below(5) as i8 - 2),
        4 => TilePattern::Diag(3 + rng.below(2) as u8, rng.below(4) as u8),
        _ => TilePattern::Scatter(rng.next_u64() as u32, 22 + rng.below(18) as u8),
    }
}

fn make_deck(rng: &mut Rng, kind: SectionKind, diff: f32) -> Deck {
    let hard = diff > 1.2;
    // Accent lands on the downbeat, `main` on the other beats, `off` between beats.
    let (accents, mains): (&[Pattern], &[Pattern]) = match kind {
        SectionKind::Calm => (&[Pattern::Fan, Pattern::Wall], &[Pattern::Fan, Pattern::Burst]),
        SectionKind::Normal => (&[Pattern::Wall, Pattern::Pincer, Pattern::Cross], &[Pattern::Fan, Pattern::Burst, Pattern::Spiral]),
        _ => {
            if hard {
                (&[Pattern::Wall, Pattern::Pincer, Pattern::Cross, Pattern::Flower, Pattern::Arc], &[Pattern::Fan, Pattern::Spiral, Pattern::Burst, Pattern::Ring])
            } else {
                (&[Pattern::Wall, Pattern::Pincer, Pattern::Cross], &[Pattern::Fan, Pattern::Spiral, Pattern::Burst])
            }
        }
    };
    let acc = rng.pick(accents);
    let main = rng.pick(mains);
    let mut third = rng.pick(mains);
    if third == main {
        third = if main == Pattern::Fan { Pattern::Burst } else { Pattern::Fan };
    }
    // Beat 3 answers the downbeat with a second accent in hot sections.
    let mid = if matches!(kind, SectionKind::Intense | SectionKind::Drop) {
        let mut m = rng.pick(accents);
        if m == acc {
            m = if acc == Pattern::Wall { Pattern::Pincer } else { Pattern::Wall };
        }
        m
    } else {
        third
    };
    let off = if rng.chance(0.5) { Pattern::Burst } else { Pattern::Fan };
    let seq = |rng: &mut Rng| -> TileSeq {
        match kind {
            SectionKind::Calm | SectionKind::Normal => match rng.below(5) {
                0 => TileSeq::SweepCols(rng.chance(0.5)),
                1 => TileSeq::Ripple(rng.chance(0.5)),
                _ => TileSeq::Single(single_tiles(rng)),
            },
            _ => match rng.below(6) {
                0 => TileSeq::SweepCols(rng.chance(0.5)),
                1 => TileSeq::SweepRows(rng.chance(0.5)),
                2 => TileSeq::Ripple(rng.chance(0.5)),
                3 => TileSeq::Flip(rng.below(2) as u8),
                _ => TileSeq::Single(single_tiles(rng)),
            },
        }
    };
    Deck {
        moves: [acc, off, main, off, mid, off, third, off],
        rot_step: rng.pick(&[0.262, 0.393, 0.524, 0.785]) * rng.sign(),
        gap_side: rng.sign(),
        soft_every: 3 + rng.below(2) as u32,
        tiles: [seq(rng), seq(rng), seq(rng)],
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
        other => other,
    }
}

/// How long before a kick the Heart announces its pulse (approach ring closing on the beat).
pub fn pulse_lead(beat_len: f32) -> f32 {
    (beat_len * 0.9).clamp(0.42, 0.6)
}

fn pulse_speed(pattern: Pattern, i: f32, kind: SectionKind, diff: f32) -> f32 {
    (215.0 + 60.0 * i) * (0.9 + 0.1 * diff) * pattern.speed_mul() * if kind == SectionKind::Calm { 0.85 } else { 1.0 }
}

/// Token bucket: fills at a rate per second, holds at most `cap_secs` seconds worth.
struct Bucket {
    tokens: f32,
    last_t: f32,
    cap_secs: f32,
}

impl Bucket {
    fn new(start: f32, cap_secs: f32) -> Self {
        Bucket { tokens: start, last_t: 0.0, cap_secs }
    }
    fn fill(&mut self, t: f32, rate: f32) {
        let dt = (t - self.last_t).clamp(0.0, 4.0);
        self.last_t = self.last_t.max(t);
        self.tokens = (self.tokens + dt * rate).min(rate * self.cap_secs);
    }
    /// Size a pulse from the tokens in hand: between the pattern's min and max, 0 if broke.
    fn pulse_count(&mut self, pattern: Pattern, diff: f32) -> u16 {
        let (lo, hi) = pattern.range();
        let hi = hi * diff.max(0.6);
        let lo = (lo * diff.clamp(0.7, 1.0)).max(3.0);
        if self.tokens < lo {
            return 0;
        }
        let give = self.tokens.min(hi).floor();
        self.tokens -= give;
        give as u16
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
            if r < 0.66 {
                EnemyKind::Chaser
            } else if r < 0.86 {
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
            } else if r < 0.86 {
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
    let travel = dur * rng.range(80.0, 170.0) * rng.sign();
    let to = (from + travel).clamp(-half * 0.96, half * 0.96);
    LevelEvent { t, lead, ev: Ev::Laser { horizontal, from, to, dur } }
}

fn shard_point(ang: f32, wobble: f32) -> (f32, f32) {
    let r = 250.0 + 110.0 * wobble.sin();
    (ang.cos() * r * 1.55, ang.sin() * r * 0.9)
}

/// Picks the rotation value for a pulse and advances the deck's running phase.
fn pulse_rot(pattern: Pattern, deck: &Deck, phase: &mut f32, uses: u32) -> f32 {
    match pattern {
        // Signed offset of the gap from the player's direction; alternates sides.
        Pattern::Wall => {
            let side = if uses % 2 == 0 { deck.gap_side } else { -deck.gap_side };
            side * (0.5 + 0.22 * ((uses / 2) % 2) as f32)
        }
        Pattern::Fan | Pattern::Burst | Pattern::Pincer | Pattern::Cross => 0.0,
        _ => {
            *phase += deck.rot_step;
            *phase
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Whole-song generation
// ---------------------------------------------------------------------------------------------

pub fn generate(an: &Analysis, user_seed: u64, diff: f32) -> Level {
    let seed = an.seed ^ user_seed.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    let mut rng = Rng::new(seed);
    let mut ev: Vec<LevelEvent> = Vec::with_capacity(an.onsets.len() + an.beats.len() + 64);
    let beat_len = if an.bpm > 1.0 { 60.0 / an.bpm } else { 0.5 };
    let norm = Norm::new(an);
    let nsec = an.sections.len().max(1);

    let t_start = an.silences.first().filter(|s| s.0 <= 0.01).map(|s| s.1).unwrap_or(0.0);
    let grace = t_start + GRACE;

    for (i, &t) in an.beats.iter().enumerate() {
        let down = i >= an.downbeat && (i - an.downbeat) % 4 == 0;
        ev.push(LevelEvent { t, lead: 0.0, ev: Ev::Beat { down } });
    }
    for o in &an.onsets {
        if !an.in_silence(o.t) {
            ev.push(LevelEvent { t: o.t, lead: 0.0, ev: Ev::Hit { band: o.band as u8, strength: o.strength } });
        }
    }

    // ---- sections: palette, one deck per section KIND (a returning chorus brings its attack back) ----
    let kind_decks = [
        make_deck(&mut rng, SectionKind::Calm, diff),
        make_deck(&mut rng, SectionKind::Normal, diff),
        make_deck(&mut rng, SectionKind::Intense, diff),
        make_deck(&mut rng, SectionKind::Drop, diff),
    ];
    let mut palette = rng.below(N_PALETTES as usize) as u8;
    let mut last_orbs = t_start;
    let mut last_heal = t_start;
    let wave_lead = (2.0 * beat_len).clamp(1.1, 1.6);
    for (idx, s) in an.sections.iter().enumerate() {
        if idx > 0 {
            palette = (palette + 1 + rng.below(N_PALETTES as usize - 1) as u8) % N_PALETTES;
        }
        ev.push(LevelEvent {
            t: s.t0,
            lead: 0.0,
            ev: Ev::Section { idx: idx as u16, kind: s.kind, intensity: s.intensity, palette },
        });
        if idx == 0 || s.t0 < grace {
            continue;
        }
        if s.kind == SectionKind::Drop {
            ev.push(LevelEvent {
                t: s.t0,
                lead: wave_lead,
                ev: Ev::Shockwave { gap: rng.angle(), gaps: if diff > 1.2 { 2 } else { 3 } },
            });
            ev.push(spawn_event(&mut rng, EnemyKind::Elite, s.t0 + 2.0 * beat_len, 0.8));
        }
        if s.kind == SectionKind::Calm || s.t0 - last_orbs > 55.0 {
            ev.push(LevelEvent { t: s.t0 + 0.8, lead: 0.0, ev: Ev::Orbs });
            last_orbs = s.t0;
        }
        if s.t0 - last_heal > 40.0 || (s.kind == SectionKind::Calm && s.t0 - last_heal > 20.0) {
            let a = rng.angle();
            ev.push(LevelEvent { t: s.t0 + 1.5, lead: 0.0, ev: Ev::Heal { x: a.cos() * 420.0, y: a.sin() * 260.0 } });
            last_heal = s.t0;
        }
    }
    let section_index = |t: f32| -> usize {
        an.sections.iter().position(|s| t >= s.t0 && t < s.t1).unwrap_or(an.sections.len().saturating_sub(1))
    };
    let kind_at = |t: f32| -> SectionKind { an.sections.get(section_index(t)).map(|s| s.kind).unwrap_or(SectionKind::Normal) };
    let level_i = |t: f32| -> f32 { norm.at(an.intensity_at(t)) };
    let calm_mul = |k: SectionKind| -> f32 { if k == SectionKind::Calm { 0.6 } else { 1.0 } };

    // ---- Heart pulses: kicks first, the beat grid fills any hole the drums leave ----
    // Candidates in time order: (time, strength, is_kick).
    let mut cand: Vec<(f32, f32, bool)> = Vec::with_capacity(an.beats.len() + an.onsets.len() / 3);
    for o in an.onsets.iter().filter(|o| o.band == Band::Low) {
        cand.push((o.t, o.strength, true));
    }
    for &b in &an.beats {
        cand.push((b, 0.5, false));
    }
    cand.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let kick_times: Vec<f32> = an.onsets.iter().filter(|o| o.band == Band::Low).map(|o| o.t).collect();
    let kick_soon = |t: f32, within: f32| -> bool {
        let i = kick_times.partition_point(|&k| k <= t + 0.02);
        i < kick_times.len() && kick_times[i] - t < within
    };
    let lead_p = pulse_lead(beat_len);
    let mut bullets = Bucket::new(BULLETS_PER_SEC * diff, 2.4);
    let mut last_pulse = -10.0f32;
    let mut phase = [rng.angle(), rng.angle(), rng.angle(), rng.angle()];
    let mut uses = [[0u32; 8]; 4];
    let mut soft_n = [0u32; 4];
    for &(t, strength, is_kick) in &cand {
        if t < grace || an.in_silence(t) {
            continue;
        }
        let kind = kind_at(t);
        let ki = kind_idx(kind);
        let target = PULSE_INTERVAL[ki] / diff.sqrt();
        let since = t - last_pulse;
        if is_kick {
            // Weak in-between kicks do not interrupt the pattern.
            if since < 0.62 * target || (strength < 0.3 && since < target) {
                continue;
            }
        } else if since < 1.4 * target || kick_soon(t, 0.3 * target) {
            continue;
        }
        let i = level_i(t);
        bullets.fill(t, BULLETS_PER_SEC * diff * energy(i) * calm_mul(kind));
        let deck = &kind_decks[ki];
        // Which half-beat of the bar is this? That picks the move.
        let bp = beat_pos(&an.beats, t) - an.downbeat as f32;
        let pos = ((bp.rem_euclid(4.0) * 2.0).round() as usize) % 8;
        let pattern = deck.moves[pos];
        let count = bullets.pulse_count(pattern, diff);
        if count == 0 {
            continue;
        }
        last_pulse = t;
        let rot = pulse_rot(pattern, deck, &mut phase[ki], uses[ki][pos]);
        uses[ki][pos] += 1;
        let soft = pos != 0 && {
            soft_n[ki] += 1;
            soft_n[ki] % deck.soft_every == 0
        };
        let speed = pulse_speed(pattern, i, kind, diff);
        ev.push(LevelEvent { t, lead: lead_p, ev: Ev::Pulse { pattern, count, speed, rot, soft } });
    }

    // ---- enemies: snares first, beats when there are none ----
    let mut ecand: Vec<(f32, f32, bool)> = Vec::new();
    for o in an.onsets.iter().filter(|o| o.band == Band::Mid) {
        ecand.push((o.t, o.strength, true));
    }
    for &b in &an.beats {
        ecand.push((b, 0.5, false));
    }
    ecand.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut foes = Bucket::new(0.5, 14.0);
    let mut last_spawn = -10.0f32;
    let spawn_lead = beat_len.clamp(0.5, 0.9);
    for &(t, strength, is_snare) in &ecand {
        if t < grace + 2.0 || an.in_silence(t) || t > an.duration - 4.0 {
            continue;
        }
        let kind = kind_at(t);
        foes.fill(t, ENEMY_POINTS_PER_SEC * diff * energy(level_i(t)) * calm_mul(kind));
        let since = t - last_spawn;
        if is_snare {
            if since < 0.9 || strength < 0.25 {
                continue;
            }
        } else if since < 4.0 || foes.tokens < 2.0 {
            continue;
        }
        let ek = pick_enemy(&mut rng, kind);
        if foes.tokens < ek.cost() {
            continue;
        }
        foes.tokens -= ek.cost();
        last_spawn = t;
        ev.push(spawn_event(&mut rng, ek, t, spawn_lead));
    }

    // ---- hi-hats: shard trails, and dart streams in the last bar of every 4 in hot sections ----
    let bars: Vec<f32> = an.beats.iter().skip(an.downbeat).step_by(4).copied().collect();
    let mut shard_ang = rng.angle();
    let shard_dir = rng.sign();
    let mut last_tick = -10.0f32;
    // Streams run in the bar that leads into a new 4-bar phrase (every 8 bars outside drops).
    let stream_bar = |t: f32, every: usize| -> bool {
        let j = bars.partition_point(|&b| b <= t);
        j > 0 && (j - 1) % every == every - 1
    };
    for o in an.onsets.iter().filter(|o| o.band == Band::High) {
        if o.t < grace || an.in_silence(o.t) {
            continue;
        }
        let kind = kind_at(o.t);
        let hot = matches!(kind, SectionKind::Intense | SectionKind::Drop);
        if hot && diff >= 0.9 && stream_bar(o.t, if kind == SectionKind::Drop || diff > 1.2 { 4 } else { 8 }) {
            if o.t - last_tick >= 0.24 / diff.sqrt() {
                last_tick = o.t;
                ev.push(LevelEvent { t: o.t, lead: 0.0, ev: Ev::Tick { speed: TICK_SPEED } });
            }
        } else if rng.chance(if hot { 0.14 } else { 0.3 }) {
            shard_ang += shard_dir * rng.range(0.25, 0.6);
            let (x, y) = shard_point(shard_ang, o.t * 0.7);
            ev.push(LevelEvent { t: o.t, lead: 0.3, ev: Ev::Shards { x, y, n: 1 + (o.strength > 0.6) as u8 } });
        }
    }

    // ---- tile-strike phrases: start on a bar line, step on the beat ----
    let step_beats: usize = if beat_len >= 0.42 { 1 } else { 2 };
    let tile_lead = (4.0 * beat_len).clamp(1.3, 2.4);
    let mut busy: Vec<(f32, f32)> = Vec::new();
    let mut next_tiles = grace + 5.0;
    let mut tile_n = [0u32; 4];
    let mut steps: Vec<(u32, TilePattern)> = Vec::with_capacity(8);
    for (j, &bt) in bars.iter().enumerate() {
        if bt < next_tiles || an.in_silence(bt) || bt > an.duration - 3.0 {
            continue;
        }
        let kind = kind_at(bt);
        let ki = kind_idx(kind);
        if TILE_INTERVAL[ki] <= 0.0 {
            continue;
        }
        let deck = &kind_decks[ki];
        let n = tile_n[ki];
        tile_n[ki] += 1;
        deck.tiles[(n % 3) as usize].steps(n / 3, &mut steps);
        let base = an.downbeat + 4 * j;
        let mut last_t = bt;
        for &(k, pat) in &steps {
            let bi = base + k as usize * step_beats;
            let t = match an.beats.get(bi) {
                Some(&t) => t,
                None => break,
            };
            if an.in_silence(t) {
                break;
            }
            last_t = t;
            ev.push(LevelEvent { t, lead: tile_lead, ev: Ev::Tiles { pattern: pat } });
        }
        busy.push((bt - tile_lead - 0.3, last_t + 0.4));
        next_tiles = last_t + TILE_INTERVAL[ki] / diff;
    }

    // ---- lasers: sustained phrases, snapped to the beat, never while tiles are winding up ----
    let nearest_beat = |t: f32| -> f32 {
        let i = an.beats.partition_point(|&b| b < t);
        let a = if i > 0 { an.beats[i - 1] } else { f32::MIN };
        let b = an.beats.get(i).copied().unwrap_or(f32::MAX);
        if (t - a).abs() <= (b - t).abs() {
            a
        } else {
            b
        }
    };
    let laser_lead = (2.0 * beat_len).clamp(1.0, 1.6);
    let mut next_laser = grace + 8.0;
    let mut lcand: Vec<(f32, f32)> = an.phrases.clone();
    // Songs without sustained phrases still get lasers: offer every 2nd bar line as a fallback.
    for (j, &bt) in bars.iter().enumerate() {
        if j % 2 == 1 {
            lcand.push((bt, bt + 4.0 * beat_len));
        }
    }
    lcand.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let n_phrases = an.phrases.len();
    let phrase_soon = |t: f32, within: f32| -> bool { an.phrases.iter().any(|p| p.0 > t && p.0 - t < within) };
    for &(a, b) in &lcand {
        let t = nearest_beat(a);
        if t < next_laser || an.in_silence(t) || t > an.duration - 4.0 {
            continue;
        }
        let kind = kind_at(t);
        let ki = kind_idx(kind);
        if LASER_INTERVAL[ki] <= 0.0 {
            continue;
        }
        let is_phrase = an.phrases.iter().any(|p| (p.0 - a).abs() < 1e-4 && (p.1 - b).abs() < 1e-4);
        if !is_phrase && n_phrases > 0 && (t < next_laser + 0.8 * LASER_INTERVAL[ki] || phrase_soon(t, 3.0)) {
            // A bar-line laser only fills in when the song has left a long gap.
            continue;
        }
        let beats_long = (((b - a) / beat_len).round()).clamp(2.0, (2.8 / beat_len).max(2.0));
        let dur = (beats_long * beat_len).clamp(0.8, 3.0);
        if diff <= 1.2 && busy.iter().any(|&(x, y)| t - laser_lead < y && t + dur > x) {
            continue;
        }
        next_laser = t + dur + LASER_INTERVAL[ki] / diff;
        ev.push(laser_event(&mut rng, t, dur, laser_lead));
    }
    let _ = nsec;

    ev.push(LevelEvent { t: an.duration, lead: 0.0, ev: Ev::End });
    ev.sort_by(|a, b| a.at().partial_cmp(&b.at()).unwrap_or(std::cmp::Ordering::Equal));

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
    decks: [Deck; 4],
    kind: SectionKind,
    palette: u8,
    section_idx: u16,
    section_t: f32,
    section_i: f32,
    last_pulse: f32,
    last_spawn: f32,
    last_tick: f32,
    next_laser: f32,
    next_tiles: f32,
    last_orbs: f32,
    last_heal: f32,
    pulse_n: u32,
    soft_n: u32,
    tile_n: u32,
    phase: f32,
    uses: [u32; 8],
    shard_ang: f32,
    pub intensity: f32,
    rate: f32,
    last_feat_t: f32,
    pending_section: bool,
    bullets: Bucket,
    foes: Bucket,
    /// Seconds per beat as currently estimated by the caller (0.5 when unknown).
    pub beat_len: f32,
}

impl LiveGen {
    pub fn new(seed: u64, diff: f32, lookahead: f32) -> Self {
        let mut rng = Rng::new(seed);
        let decks = [
            make_deck(&mut rng, SectionKind::Calm, diff),
            make_deck(&mut rng, SectionKind::Normal, diff),
            make_deck(&mut rng, SectionKind::Intense, diff),
            make_deck(&mut rng, SectionKind::Drop, diff),
        ];
        let palette = rng.below(N_PALETTES as usize) as u8;
        LiveGen {
            rng,
            diff,
            lookahead,
            decks,
            kind: SectionKind::Normal,
            palette,
            section_idx: 0,
            section_t: 0.0,
            section_i: 0.5,
            last_pulse: -10.0,
            last_spawn: -10.0,
            last_tick: -10.0,
            next_laser: 12.0,
            next_tiles: 9.0,
            last_orbs: 0.0,
            last_heal: 0.0,
            pulse_n: 0,
            soft_n: 0,
            tile_n: 0,
            phase: 0.0,
            uses: [0; 8],
            shard_ang: 0.0,
            intensity: 0.4,
            rate: 0.0,
            last_feat_t: 0.0,
            pending_section: true,
            bullets: Bucket::new(BULLETS_PER_SEC * diff, 2.4),
            foes: Bucket::new(0.5, 14.0),
            beat_len: 0.5,
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
            self.palette = (self.palette + 1 + self.rng.below(N_PALETTES as usize - 1) as u8) % N_PALETTES;
            self.pulse_n = 0;
            out.push(LevelEvent {
                t,
                lead: 0.0,
                ev: Ev::Section { idx: self.section_idx, kind: self.kind, intensity: self.intensity, palette: self.palette },
            });
            if self.kind == SectionKind::Drop {
                let (te, lead) = self.sched(t, 1.2);
                out.push(LevelEvent { t: te, lead, ev: Ev::Shockwave { gap: self.rng.angle(), gaps: if self.diff > 1.2 { 2 } else { 3 } } });
                let (te, lead) = self.sched(t + 1.0, 0.8);
                out.push(spawn_event(&mut self.rng, EnemyKind::Elite, te, lead));
            }
            if (self.kind == SectionKind::Calm && t - self.last_orbs > 25.0) || t - self.last_orbs > 60.0 {
                self.last_orbs = t;
                out.push(LevelEvent { t: t + 0.8, lead: 0.0, ev: Ev::Orbs });
            }
            if t - self.last_heal > 40.0 {
                self.last_heal = t;
                let a = self.rng.angle();
                out.push(LevelEvent { t: t + 1.5, lead: 0.0, ev: Ev::Heal { x: a.cos() * 420.0, y: a.sin() * 260.0 } });
            }
        }
    }

    pub fn on_onset(&mut self, o: &Onset, out: &mut Vec<LevelEvent>) {
        self.rate += 0.5;
        out.push(LevelEvent { t: o.t, lead: 0.0, ev: Ev::Hit { band: o.band as u8, strength: o.strength } });
        // Live intensity is already a rolling, self-normalising value.
        let i = self.intensity;
        let ki = kind_idx(self.kind);
        let calm = if self.kind == SectionKind::Calm { 0.6 } else { 1.0 };
        let hot = matches!(self.kind, SectionKind::Intense | SectionKind::Drop);
        match o.band {
            Band::Low => {
                let target = PULSE_INTERVAL[ki] / self.diff.sqrt();
                let since = o.t - self.last_pulse;
                if since >= 0.62 * target && !(o.strength < 0.3 && since < target) {
                    self.bullets.fill(o.t, BULLETS_PER_SEC * self.diff * energy(i) * calm);
                    // No bar grid live: walk the motif one beat per pulse.
                    let pos = ((self.pulse_n * 2) % 8) as usize;
                    let deck = self.decks[ki];
                    let pattern = deck.moves[pos];
                    let count = self.bullets.pulse_count(pattern, self.diff);
                    if count > 0 {
                        self.last_pulse = o.t;
                        self.pulse_n += 1;
                        let rot = pulse_rot(pattern, &deck, &mut self.phase, self.uses[pos]);
                        self.uses[pos] += 1;
                        let soft = pos != 0 && {
                            self.soft_n += 1;
                            self.soft_n % deck.soft_every == 0
                        };
                        // Only delayed (look-ahead) live mode knows a kick early enough to announce it.
                        let lead = if self.lookahead >= 0.5 { pulse_lead(self.beat_len) } else { 0.0 };
                        let speed = pulse_speed(pattern, i, self.kind, self.diff);
                        out.push(LevelEvent { t: o.t, lead, ev: Ev::Pulse { pattern, count, speed, rot, soft } });
                    }
                }
                if TILE_INTERVAL[ki] > 0.0 && o.t >= self.next_tiles && o.strength > 0.4 {
                    let deck = self.decks[ki];
                    let mut steps = Vec::with_capacity(8);
                    deck.tiles[(self.tile_n % 3) as usize].steps(self.tile_n / 3, &mut steps);
                    self.tile_n += 1;
                    let lead_t = (4.0 * self.beat_len).clamp(1.3, 2.4);
                    let (t0, lead) = self.sched(o.t, lead_t);
                    let step = self.beat_len.max(0.42);
                    let mut last = t0;
                    for &(k, pat) in &steps {
                        last = t0 + k as f32 * step;
                        out.push(LevelEvent { t: last, lead, ev: Ev::Tiles { pattern: pat } });
                    }
                    self.next_tiles = last + TILE_INTERVAL[ki] / self.diff;
                    self.next_laser = self.next_laser.max(last + 1.0);
                } else if LASER_INTERVAL[ki] > 0.0 && o.t >= self.next_laser && o.strength > 0.5 {
                    let (t, lead) = self.sched(o.t, (2.0 * self.beat_len).clamp(1.0, 1.6));
                    let dur = (self.rng.range(2.0, 4.0).round() * self.beat_len).clamp(0.9, 2.8);
                    self.next_laser = t + dur + LASER_INTERVAL[ki] / self.diff;
                    self.next_tiles = self.next_tiles.max(t + dur + 1.0);
                    out.push(laser_event(&mut self.rng, t, dur, lead));
                }
            }
            Band::Mid => {
                self.foes.fill(o.t, ENEMY_POINTS_PER_SEC * self.diff * energy(i) * calm);
                if o.t - self.last_spawn < 0.9 || o.strength < 0.25 {
                    return;
                }
                let kind = pick_enemy(&mut self.rng, self.kind);
                if self.foes.tokens < kind.cost() {
                    return;
                }
                self.foes.tokens -= kind.cost();
                self.last_spawn = o.t;
                let (t, lead) = self.sched(o.t, 0.6);
                out.push(spawn_event(&mut self.rng, kind, t, lead));
            }
            Band::High => {
                // Dart streams: the last two seconds of every eight in hot sections.
                let streaming = hot && self.diff >= 0.9 && (o.t - self.section_t).rem_euclid(8.0) > 6.0;
                if streaming {
                    if o.t - self.last_tick >= 0.24 / self.diff.sqrt() {
                        self.last_tick = o.t;
                        out.push(LevelEvent { t: o.t, lead: 0.0, ev: Ev::Tick { speed: TICK_SPEED } });
                    }
                } else if self.rng.chance(if hot { 0.14 } else { 0.3 }) {
                    self.shard_ang += self.rng.range(0.25, 0.6);
                    let (x, y) = shard_point(self.shard_ang, o.t * 0.7);
                    let (t, lead) = self.sched(o.t, 0.3);
                    out.push(LevelEvent { t, lead, ev: Ev::Shards { x, y, n: 1 } });
                }
            }
        }
    }
}
