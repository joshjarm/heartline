//! Procedural synthwave soundtrack: the game's built-in "standard" tracks.
//!
//! Everything is synthesised from scratch (polyBLEP oscillators, TPT state-variable filters,
//! noise drums, a feedback-delay-network reverb, a ping-pong delay and kick sidechain), so the
//! game ships no audio assets. Every track has a real song form (intro, verse, build, chorus,
//! breakdown, chorus, outro) so the level analysis finds varied sections, and the buffer loops
//! seamlessly: release and effect tails that run past the end are folded back onto the start,
//! and the stateful master processors are warmed up on the end of the loop.

use std::f32::consts::{PI, TAU};
use std::path::PathBuf;

use crate::decode::Track;
use crate::rng::Rng;

/// Number of built-in tracks.
pub const THEME_COUNT: usize = 3;

const SR: u32 = 44_100;
const SRF: f32 = SR as f32;
/// Extra render length for release / effect tails; folded back onto the loop start.
const TAIL_SECS: f64 = 5.0;
/// Rest marker in bass patterns.
const R: i32 = i32::MIN;
/// Master ceiling (-1.2 dBFS) and target RMS of the choruses (-14 dBFS).
const CEIL: f32 = 0.871;
const TARGET_RMS: f32 = 0.2;

/// Display name of a built-in track, without generating it.
pub fn theme_name(variant: usize) -> &'static str {
    style(variant % THEME_COUNT).name
}

/// Synthesise built-in track `variant % THEME_COUNT`. Deterministic. `path` is left empty.
pub fn theme(variant: usize) -> Track {
    let st = style(variant % THEME_COUNT);
    let samples = render(&st);
    Track { name: st.name.to_string(), path: PathBuf::new(), sr: SR, samples }
}

// ---------------------------------------------------------------------------------------------
// Song data
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Intro,
    Verse,
    Build,
    Chorus,
    Break,
    Outro,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Groove {
    /// Four-on-the-floor, offbeat hats, open hats in the chorus.
    Four,
    /// Kick on 1 and 3 (plus a pickup in the chorus), straight 8th hats.
    Half,
    /// Four-on-the-floor with driving 16th hats and a clap layer.
    Drive,
}

/// Bass root followed by four pad notes (MIDI).
type Chord = [i32; 5];

/// A monophonic synth voice.
#[derive(Clone, Copy)]
struct Tone {
    saw: f32,
    saw2: f32,
    detune: f32,
    sqr: f32,
    pw: f32,
    sine: f32,
    cut: f32,
    env_amt: f32,
    f_decay: f32,
    q: f32,
    key: f32,
    a: f32,
    d: f32,
    s: f32,
    r: f32,
    vib: f32,
    vib_rate: f32,
    vib_delay: f32,
    glide: f32,
    /// Filter-envelope level retriggered on legato notes.
    legato_f: f32,
    /// 4-pole high-pass on the oscillators (0 = off).
    hp: f32,
}

const TONE0: Tone = Tone {
    saw: 0.0,
    saw2: 0.0,
    detune: 0.0,
    sqr: 0.0,
    pw: 0.5,
    sine: 0.0,
    cut: 1000.0,
    env_amt: 0.0,
    f_decay: 0.1,
    q: 0.7,
    key: 0.0,
    a: 0.005,
    d: 0.2,
    s: 1.0,
    r: 0.1,
    vib: 0.0,
    vib_rate: 5.0,
    vib_delay: 0.2,
    glide: 0.0,
    legato_f: 0.5,
    hp: 0.0,
};

struct Style {
    name: &'static str,
    bpm: f64,
    seed: u64,
    form: &'static [(Part, u32)],
    verse: [Chord; 4],
    chorus: [Chord; 4],
    groove: Groove,
    bass_pat: [i32; 16],
    bass_gate: f64,
    bass: Tone,
    arp_pat: &'static [usize],
    arp_rate: u32,
    arp_oct: i32,
    arp_from: u32,
    arp: Tone,
    lead: Tone,
    legato: bool,
    /// (bar, step, length in 16ths, MIDI) over the 8-bar chorus.
    motif: &'static [(u32, u32, u32, i32)],
    /// (bar, length in bars, MIDI) sustained breakdown phrase.
    sustain: &'static [(u32, u32, i32)],
    pad_detune: f32,
    pad_cut: f32,
    kick_decay: f32,
    kick_end: f32,
    clap: bool,
    rev_t60: f32,
    dly_steps: f64,
    dly_fb: f32,
}

fn style(v: usize) -> Style {
    match v {
        0 => Style {
            name: "HEARTLINE THEME",
            bpm: 112.0,
            seed: 0x4845_4152_544c_4e31,
            form: &[
                (Part::Intro, 8),
                (Part::Verse, 8),
                (Part::Build, 4),
                (Part::Chorus, 8),
                (Part::Break, 8),
                (Part::Chorus, 8),
                (Part::Outro, 8),
            ],
            // D minor: Dm Bb F C / Bb F C Dm
            verse: [[38, 57, 62, 65, 69], [34, 58, 62, 65, 70], [41, 57, 60, 65, 69], [36, 55, 60, 64, 67]],
            chorus: [[34, 58, 62, 65, 70], [41, 57, 60, 65, 69], [36, 55, 60, 64, 67], [38, 57, 62, 65, 69]],
            groove: Groove::Four,
            bass_pat: [0, 0, 12, 0, 0, 0, 12, 0, 0, 0, 12, 0, 0, 12, 0, 12],
            bass_gate: 1.0,
            bass: Tone { saw: 0.7, sqr: 0.3, sine: 0.6, cut: 260.0, env_amt: 5.0, f_decay: 0.08, q: 1.3, key: 0.5, a: 0.004, d: 0.2, s: 0.85, r: 0.03, legato_f: 1.0, ..TONE0 },
            arp_pat: &[0, 1, 2, 3, 4, 3, 2, 1, 0, 1, 2, 3, 4, 5, 4, 2],
            arp_rate: 1,
            arp_oct: 12,
            arp_from: 4,
            arp: Tone { saw: 0.6, sqr: 0.4, pw: 0.25, cut: 900.0, env_amt: 4.0, f_decay: 0.12, q: 1.8, key: 0.3, a: 0.002, d: 0.16, s: 0.0, r: 0.08, ..TONE0 },
            lead: Tone { saw: 0.55, saw2: 0.55, detune: 9.0, sqr: 0.3, cut: 2200.0, env_amt: 1.2, f_decay: 0.25, q: 1.1, key: 0.5, a: 0.012, d: 0.4, s: 0.8, r: 0.22, vib: 0.2, vib_rate: 5.4, vib_delay: 0.22, glide: 0.035, ..TONE0 },
            legato: true,
            motif: &[
                (0, 0, 6, 74), (0, 6, 2, 77), (0, 8, 4, 72), (0, 12, 4, 74),
                (1, 0, 6, 72), (1, 6, 2, 69), (1, 8, 6, 77), (1, 14, 2, 76),
                (2, 0, 6, 79), (2, 6, 2, 76), (2, 8, 4, 74), (2, 12, 4, 72),
                (3, 0, 10, 74), (3, 10, 2, 72), (3, 12, 4, 69),
                (4, 0, 6, 74), (4, 6, 2, 77), (4, 8, 4, 79), (4, 12, 4, 81),
                (5, 0, 6, 84), (5, 6, 2, 81), (5, 8, 8, 77),
                (6, 0, 4, 79), (6, 4, 4, 76), (6, 8, 4, 79), (6, 12, 4, 84),
                (7, 0, 12, 81), (7, 12, 2, 77), (7, 14, 2, 76),
            ],
            sustain: &[(0, 2, 69), (2, 1, 72), (3, 1, 76), (4, 1, 77), (5, 1, 74), (6, 1, 72)],
            pad_detune: 16.0,
            pad_cut: 4200.0,
            kick_decay: 0.11,
            kick_end: 50.0,
            clap: false,
            rev_t60: 2.2,
            dly_steps: 3.0,
            dly_fb: 0.38,
        },
        1 => Style {
            name: "NEON TIDE",
            bpm: 96.0,
            seed: 0x4e45_4f4e_5449_4445,
            form: &[
                (Part::Intro, 8),
                (Part::Verse, 8),
                (Part::Build, 4),
                (Part::Chorus, 8),
                (Part::Break, 8),
                (Part::Chorus, 8),
                (Part::Outro, 4),
            ],
            // Eb major: Ebmaj7 Cm9 Abmaj7 Bbsus / Abmaj7 Bb Gm7 Cm7
            verse: [[39, 58, 62, 63, 67], [36, 58, 62, 63, 67], [32, 56, 60, 63, 67], [34, 58, 60, 63, 65]],
            chorus: [[32, 56, 60, 63, 67], [34, 58, 62, 65, 70], [31, 58, 62, 65, 67], [36, 58, 60, 63, 67]],
            groove: Groove::Half,
            bass_pat: [0, R, 0, R, 12, R, 0, R, 0, R, 7, R, 12, R, 7, R],
            bass_gate: 2.0,
            bass: Tone { saw: 0.45, sine: 0.8, cut: 320.0, env_amt: 2.5, f_decay: 0.2, q: 0.9, key: 0.4, a: 0.006, d: 0.4, s: 0.8, r: 0.06, legato_f: 1.0, ..TONE0 },
            arp_pat: &[0, 2, 1, 3, 2, 4, 3, 5],
            arp_rate: 2,
            arp_oct: 12,
            arp_from: 2,
            arp: Tone { saw: 0.2, sqr: 0.5, cut: 1300.0, env_amt: 2.5, f_decay: 0.3, q: 0.9, key: 0.2, a: 0.003, d: 0.45, s: 0.0, r: 0.2, ..TONE0 },
            lead: Tone { saw: 0.3, saw2: 0.3, detune: 6.0, sqr: 0.55, sine: 0.3, cut: 1700.0, env_amt: 1.0, f_decay: 0.4, q: 0.9, key: 0.5, a: 0.03, d: 0.6, s: 0.8, r: 0.4, vib: 0.25, vib_rate: 5.0, vib_delay: 0.3, glide: 0.07, ..TONE0 },
            legato: true,
            motif: &[
                (0, 0, 4, 75), (0, 4, 4, 77), (0, 8, 8, 79),
                (1, 0, 6, 77), (1, 6, 2, 74), (1, 8, 8, 70),
                (2, 0, 4, 74), (2, 4, 4, 77), (2, 8, 6, 82), (2, 14, 2, 79),
                (3, 0, 12, 79), (3, 12, 4, 75),
                (4, 0, 6, 84), (4, 6, 2, 82), (4, 8, 8, 79),
                (5, 0, 4, 77), (5, 4, 4, 79), (5, 8, 8, 82),
                (6, 0, 8, 86), (6, 8, 4, 82), (6, 12, 4, 79),
                (7, 0, 10, 79), (7, 10, 6, 75),
            ],
            sustain: &[(0, 1, 79), (1, 1, 75), (2, 1, 72), (3, 1, 77), (4, 2, 82), (6, 1, 84)],
            pad_detune: 11.0,
            pad_cut: 3200.0,
            kick_decay: 0.14,
            kick_end: 46.0,
            clap: false,
            rev_t60: 3.2,
            dly_steps: 3.0,
            dly_fb: 0.45,
        },
        _ => Style {
            name: "CHROME CIRCUIT",
            bpm: 126.0,
            seed: 0x4348_524f_4d45_4343,
            form: &[
                (Part::Intro, 8),
                (Part::Verse, 8),
                (Part::Build, 8),
                (Part::Chorus, 16),
                (Part::Break, 8),
                (Part::Chorus, 8),
                (Part::Outro, 8),
            ],
            // C# minor: C#m A F#m G# / C#m A D G#
            verse: [[37, 56, 61, 64, 68], [33, 57, 61, 64, 69], [30, 57, 61, 66, 69], [32, 56, 60, 63, 68]],
            chorus: [[37, 56, 61, 64, 68], [33, 57, 61, 64, 69], [38, 57, 62, 66, 69], [32, 56, 60, 63, 68]],
            groove: Groove::Drive,
            bass_pat: [0, 0, 0, 12, 0, 0, 0, 12, 0, 0, 0, 12, 0, 0, 12, 0],
            bass_gate: 1.0,
            bass: Tone { saw: 0.8, sqr: 0.35, pw: 0.3, sine: 0.5, cut: 200.0, env_amt: 9.0, f_decay: 0.06, q: 2.2, key: 0.5, a: 0.003, d: 0.15, s: 0.8, r: 0.025, legato_f: 1.0, ..TONE0 },
            arp_pat: &[0, 1, 2, 3, 1, 2, 3, 4, 2, 3, 4, 5, 3, 4, 5, 4],
            arp_rate: 1,
            arp_oct: 12,
            arp_from: 4,
            arp: Tone { saw: 0.3, sqr: 0.7, pw: 0.15, cut: 1200.0, env_amt: 4.0, f_decay: 0.07, q: 2.4, key: 0.3, a: 0.002, d: 0.12, s: 0.0, r: 0.06, ..TONE0 },
            lead: Tone { saw: 0.65, saw2: 0.65, detune: 14.0, sqr: 0.35, pw: 0.35, cut: 2600.0, env_amt: 2.0, f_decay: 0.15, q: 1.4, key: 0.5, a: 0.006, d: 0.25, s: 0.7, r: 0.18, vib: 0.15, vib_rate: 6.0, vib_delay: 0.18, glide: 0.03, ..TONE0 },
            legato: false,
            motif: &[
                (0, 0, 3, 73), (0, 3, 3, 76), (0, 6, 4, 80), (0, 10, 2, 78), (0, 12, 4, 76),
                (1, 0, 3, 76), (1, 3, 3, 73), (1, 6, 6, 69), (1, 12, 4, 71),
                (2, 0, 3, 74), (2, 3, 3, 78), (2, 6, 6, 81), (2, 12, 2, 80), (2, 14, 2, 78),
                (3, 0, 8, 80), (3, 8, 4, 75), (3, 12, 4, 72),
                (4, 0, 3, 85), (4, 3, 3, 83), (4, 6, 4, 80), (4, 10, 2, 76), (4, 12, 4, 80),
                (5, 0, 6, 81), (5, 6, 2, 80), (5, 8, 8, 76),
                (6, 0, 3, 78), (6, 3, 3, 81), (6, 6, 6, 86), (6, 12, 4, 85),
                (7, 0, 8, 84), (7, 8, 8, 80),
            ],
            sustain: &[(0, 2, 80), (2, 1, 81), (3, 1, 84), (4, 1, 80), (5, 1, 76), (6, 1, 78)],
            pad_detune: 20.0,
            pad_cut: 3800.0,
            kick_decay: 0.09,
            kick_end: 52.0,
            clap: true,
            rev_t60: 1.8,
            dly_steps: 3.0,
            dly_fb: 0.33,
        },
    }
}

// ---------------------------------------------------------------------------------------------
// DSP building blocks
// ---------------------------------------------------------------------------------------------

#[inline]
fn mtof(m: f32) -> f32 {
    440.0 * ((m - 69.0) / 12.0).exp2()
}

#[inline]
fn blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

#[inline]
fn saw(p: f32, dt: f32) -> f32 {
    2.0 * p - 1.0 - blep(p, dt)
}

#[inline]
fn pulse(p: f32, dt: f32, pw: f32) -> f32 {
    let mut q = p - pw;
    if q < 0.0 {
        q += 1.0;
    }
    let v = if p < pw { 1.0 } else { -1.0 };
    // minus the DC of a non-square pulse, so enveloped notes do not thump
    v + blep(p, dt) - blep(q, dt) + 1.0 - 2.0 * pw
}

/// Fast sine of a phase in [0, 1) (parabolic approximation, error < 0.1%).
#[inline]
fn sin01(p: f32) -> f32 {
    let u = if p < 0.5 { 2.0 * p } else { 2.0 * p - 2.0 };
    let y = 4.0 * u * (1.0 - u.abs());
    0.225 * (y * y.abs() - y) + y
}

/// Cheap tanh-like soft clipper.
#[inline]
fn soft(x: f32) -> f32 {
    let x = x.clamp(-3.0, 3.0);
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

#[inline]
fn decay_coef(secs: f32) -> f32 {
    (-1.0 / (secs.max(1e-4) * SRF)).exp()
}

/// Topology-preserving state-variable filter (Zavalishin).
#[derive(Clone, Copy)]
struct Svf {
    ic1: f32,
    ic2: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
}

impl Svf {
    fn new(fc: f32, q: f32) -> Self {
        let mut s = Svf { ic1: 0.0, ic2: 0.0, a1: 0.0, a2: 0.0, a3: 0.0, k: 1.0 };
        s.set(fc, q);
        s
    }
    #[inline]
    fn set(&mut self, fc: f32, q: f32) {
        let g = (PI * fc.clamp(15.0, 0.45 * SRF) / SRF).tan();
        self.k = 1.0 / q;
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }
    /// Returns (low, band, high).
    #[inline]
    fn tick(&mut self, v0: f32) -> (f32, f32, f32) {
        let v3 = v0 - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        (v2, v1, v0 - self.k * v1 - v2)
    }
    #[inline]
    fn lp(&mut self, x: f32) -> f32 {
        self.tick(x).0
    }
    #[inline]
    fn bp(&mut self, x: f32) -> f32 {
        self.tick(x).1
    }
    #[inline]
    fn hp(&mut self, x: f32) -> f32 {
        self.tick(x).2
    }
}

/// Audio-rate white noise (xorshift32), seeded from the song RNG.
struct Noise(u32);

impl Noise {
    #[inline]
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x as i32) as f32 * (1.0 / 2_147_483_648.0)
    }
}

/// Render buffers: dry stereo bus, reverb and delay sends, and the kick sidechain envelope.
struct Mix {
    l: Vec<f32>,
    r: Vec<f32>,
    rev: Vec<f32>,
    dly: Vec<f32>,
    sc: Vec<f32>,
    /// Loop length in frames.
    n: usize,
    /// Buffer length (loop + tail).
    len: usize,
}

impl Mix {
    #[inline]
    fn sc(&self, i: usize) -> f32 {
        if i < self.n {
            self.sc[i]
        } else {
            self.sc[i - self.n]
        }
    }
}

#[inline]
fn smp(t: f64) -> usize {
    (t.max(0.0) * SR as f64).round() as usize
}

/// Balance pan: centre is unity on both sides.
#[inline]
fn pan2(x: f32, pan: f32) -> (f32, f32) {
    (x * (1.0 - pan.max(0.0)), x * (1.0 + pan.min(0.0)))
}

/// Run a one-shot generator `f(k) -> (l, r, reverb send)` into the mix.
fn hit<F: FnMut(usize) -> (f32, f32, f32)>(mix: &mut Mix, s0: usize, len: usize, mut f: F) {
    let end = (s0 + len).min(mix.len);
    for (k, i) in (s0..end).enumerate() {
        let (l, r, v) = f(k);
        mix.l[i] += l;
        mix.r[i] += r;
        mix.rev[i] += v;
    }
}

// ---------------------------------------------------------------------------------------------
// Drums
// ---------------------------------------------------------------------------------------------

fn kick(mix: &mut Mix, s0: usize, vel: f32, decay: f32, f_end: f32, nz: &mut Noise) {
    let len = (decay * 6.0 * SRF) as usize;
    let (pk, ak, ck) = (decay_coef(0.026), decay_coef(decay), decay_coef(0.002));
    let (mut ph, mut pe, mut ae, mut ce) = (0.0f32, 1.0f32, 1.0f32, 1.0f32);
    let att = 1.0 / (0.0008 * SRF);
    let mut click = Svf::new(2500.0, 0.8);
    hit(mix, s0, len, |k| {
        let f = f_end * (1.0 + 3.4 * pe);
        pe *= pk;
        ph += f / SRF;
        if ph >= 1.0 {
            ph -= 1.0;
        }
        let a = (k as f32 * att).min(1.0) * ae;
        ae *= ak;
        let c = if k < 900 {
            ce *= ck;
            click.bp(nz.next()) * ce * 1.2
        } else {
            0.0
        };
        let x = soft(sin01(ph) * a * 1.5 + c) * vel * 0.9;
        (x, x, 0.0)
    });
}

fn snare(mix: &mut Mix, s0: usize, vel: f32, tail: f32, clap: bool, nz: &mut Noise) {
    let len = (0.36 * SRF) as usize;
    let gate = (0.2 * SRF) as usize;
    let fade = (0.012 * SRF) as usize;
    let mut snap = Svf::new(1100.0, 0.7);
    let mut tl = Svf::new(2200.0, 0.55);
    let mut tr = Svf::new(2200.0, 0.55);
    let mut cl = Svf::new(1350.0, 1.6);
    let (pk, bk, sk) = (decay_coef(0.02), decay_coef(0.055), decay_coef(0.1));
    let (mut ph, mut pe, mut be, mut se) = (0.0f32, 1.0f32, 1.0f32, 1.0f32);
    let burst = (0.0105 * SRF) as usize;
    let ck = decay_coef(0.004);
    let ct = decay_coef(0.07);
    let mut ce = 1.0f32;
    // keep the snare out of the kick band (30-160 Hz)
    let (mut hl0, mut hl1, mut hr0, mut hr1) = (Svf::new(230.0, 0.6), Svf::new(230.0, 0.6), Svf::new(230.0, 0.6), Svf::new(230.0, 0.6));
    hit(mix, s0, len, |k| {
        let f = 195.0 + 60.0 * pe;
        pe *= pk;
        ph += f / SRF;
        if ph >= 1.0 {
            ph -= 1.0;
        }
        let body = sin01(ph) * be * 0.55;
        be *= bk;
        let n0 = nz.next();
        let sn = snap.hp(n0) * se * 0.7;
        se *= sk;
        // Gated "reverb": a dense, flat noise tail that is cut off abruptly.
        let g = if k < gate {
            (k as f32 / 60.0).min(1.0) * (0.55 - 0.25 * k as f32 / gate as f32)
        } else if k < gate + fade {
            0.3 * (1.0 - (k - gate) as f32 / fade as f32)
        } else {
            0.0
        };
        let (gl, gr) = if g > 0.0 { (tl.bp(nz.next()) * g * tail, tr.bp(nz.next()) * g * tail) } else { (0.0, 0.0) };
        let mut c = 0.0;
        if clap {
            if k < 3 * burst {
                if k % burst == 0 {
                    ce = 1.0;
                }
                ce *= ck;
            } else {
                if k == 3 * burst {
                    ce = 0.8;
                }
                ce *= ct;
            }
            c = cl.bp(n0) * ce * 1.4;
        }
        let m = (body + sn + c) * vel;
        let ol = hl1.hp(hl0.hp(m + gl * vel));
        let or = hr1.hp(hr0.hp(m + gr * vel));
        (ol, or, 0.35 * 0.5 * (ol + or))
    });
}

fn hat(mix: &mut Mix, s0: usize, vel: f32, open: bool, pan: f32, nz: &mut Noise) {
    let decay = if open { 0.15 } else { 0.026 };
    let len = (decay * 6.0 * SRF) as usize;
    let mut hp = Svf::new(7200.0, 0.8);
    let mut pk = Svf::new(10_500.0, 2.0);
    let ek = decay_coef(decay);
    let mut e = 1.0f32;
    hit(mix, s0, len, |k| {
        let a = (k as f32 / 40.0).min(1.0) * e;
        e *= ek;
        let n = nz.next();
        let x = (hp.hp(n) + 0.5 * pk.bp(n)) * a * vel;
        let (l, r) = pan2(x, pan);
        (l, r, x * 0.06)
    });
}

fn crash(mix: &mut Mix, s0: usize, vel: f32, nz: &mut Noise) {
    let len = (2.6 * SRF) as usize;
    let mut hl = Svf::new(4200.0, 0.7);
    let mut hr = Svf::new(4200.0, 0.7);
    let ek = decay_coef(0.75);
    let mut e = 1.0f32;
    hit(mix, s0, len, |k| {
        let a = (k as f32 / 80.0).min(1.0) * e;
        e *= ek;
        let (l, r) = (hl.hp(nz.next()) * a * vel, hr.hp(nz.next()) * a * vel);
        (l, r, 0.2 * (l + r))
    });
}

fn tom(mix: &mut Mix, s0: usize, f0: f32, vel: f32, pan: f32, nz: &mut Noise) {
    let len = (0.9 * SRF) as usize;
    let (pk, ak, nk) = (decay_coef(0.09), decay_coef(0.16), decay_coef(0.008));
    let (mut ph, mut pe, mut ae, mut ne) = (0.0f32, 1.0f32, 1.0f32, 1.0f32);
    hit(mix, s0, len, |k| {
        let f = f0 * (0.65 + 0.35 * pe);
        pe *= pk;
        ph += f / SRF;
        if ph >= 1.0 {
            ph -= 1.0;
        }
        let a = (k as f32 / 30.0).min(1.0) * ae;
        ae *= ak;
        let x = (sin01(ph) * a + nz.next() * ne * 0.2) * vel;
        ne *= nk;
        let (l, r) = pan2(x, pan);
        (l, r, x * 0.3)
    });
}

/// Low sine "boom" that marks a drop.
fn boom(mix: &mut Mix, s0: usize, vel: f32) {
    let len = (2.4 * SRF) as usize;
    let (pk, ak) = (decay_coef(0.25), decay_coef(0.42));
    let (mut ph, mut pe, mut ae) = (0.0f32, 1.0f32, 1.0f32);
    hit(mix, s0, len, |k| {
        let f = 36.0 + 26.0 * pe;
        pe *= pk;
        ph += f / SRF;
        if ph >= 1.0 {
            ph -= 1.0;
        }
        let a = (k as f32 / 60.0).min(1.0) * ae;
        ae *= ak;
        let x = sin01(ph) * a * vel;
        (x, x, 0.0)
    });
}

/// Filtered noise sweep from `s0` to `s1`. `high` selects a high-passed "reverse cymbal".
fn riser(mix: &mut Mix, s0: usize, s1: usize, f_lo: f32, f_hi: f32, vel: f32, high: bool, nz: &mut Noise) {
    let len = s1.saturating_sub(s0).max(1);
    let fade = (0.008 * SRF) as usize;
    let mut fl = Svf::new(f_lo, 1.2);
    let mut fr = Svf::new(f_lo, 1.2);
    let ratio = f_hi / f_lo;
    hit(mix, s0, len, |k| {
        let x = k as f32 / len as f32;
        if k % 32 == 0 && !high {
            let fc = f_lo * ratio.powf(x);
            fl.set(fc, 1.2);
            fr.set(fc, 1.2);
        } else if k == 0 {
            fl.set(f_hi, 0.7);
            fr.set(f_hi, 0.7);
        }
        let mut a = if high { x * x * x } else { x * x } * vel;
        if k + fade > len {
            a *= (len - k) as f32 / fade as f32;
        }
        let (nl, nr) = (nz.next(), nz.next());
        let (l, r) = if high { (fl.hp(nl), fr.hp(nr)) } else { (fl.bp(nl), fr.bp(nr)) };
        (l * a, r * a, 0.25 * (l + r) * a)
    });
}

// ---------------------------------------------------------------------------------------------
// Tonal instruments
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Note {
    t: f64,
    dur: f64,
    midi: f32,
    vel: f32,
    pan: f32,
}

struct Bus {
    gain: f32,
    duck: f32,
    rev: f32,
    dly: f32,
}

/// Render a monophonic lane: one oscillator / filter / envelope state carried across notes, so
/// retriggers never click and contiguous notes glide (portamento) without re-attacking.
fn lane(mix: &mut Mix, notes: &[Note], tone: &Tone, bus: &Bus, cut_mod: &dyn Fn(f64) -> f32) {
    if notes.is_empty() {
        return;
    }
    const CTL: usize = 16;
    let (mut p1, mut p2) = (0.0f32, 0.37f32);
    let mut env = 0.0f32;
    let mut attacking = true;
    let mut fenv = 0.0f32;
    let mut svf = Svf::new(tone.cut, tone.q);
    let mut hp1 = Svf::new(tone.hp.max(20.0), 0.6);
    let mut hp2 = Svf::new(tone.hp.max(20.0), 0.6);
    let mut cur = notes[0].midi;
    let a_inc = 1.0 / (tone.a * SRF).max(1.0);
    let (d_mul, r_mul, f_mul) = (decay_coef(tone.d), decay_coef(tone.r), decay_coef(tone.f_decay));
    let glide_k = if tone.glide > 0.0 { 1.0 - (-(CTL as f32) / (tone.glide * SRF)).exp() } else { 1.0 };
    let det = (tone.detune / 1200.0).exp2();
    let rel_len = (tone.r * 7.0 * SRF) as usize;
    let vib_inc = tone.vib_rate * CTL as f32 / SRF;
    let mut vph = 0.0f32;
    let mut prev_end = f64::NEG_INFINITY;
    let mut dt = mtof(cur) / SRF;
    for (ni, n) in notes.iter().enumerate() {
        let s0 = smp(n.t);
        let gate = smp(n.t + n.dur).saturating_sub(s0).max(1);
        let legato = (n.t - prev_end).abs() < 1e-4;
        prev_end = n.t + n.dur;
        let mut seg = gate + rel_len;
        if let Some(nx) = notes.get(ni + 1) {
            seg = seg.min(smp(nx.t).saturating_sub(s0));
        }
        if legato {
            fenv = fenv.max(tone.legato_f);
        } else {
            cur = n.midi;
            attacking = true;
            fenv = 1.0;
            vph = 0.0;
        }
        let end = (s0 + seg).min(mix.len);
        let (gl, gr) = pan2(bus.gain * n.vel, n.pan);
        let mut k = 0usize;
        for i in s0..end {
            if k % CTL == 0 {
                cur += (n.midi - cur) * glide_k;
                let ts = k as f32 / SRF;
                let mut semi = cur;
                if tone.vib > 0.0 && ts > tone.vib_delay {
                    let depth = ((ts - tone.vib_delay) / 0.4).min(1.0) * tone.vib;
                    semi += depth * (vph * TAU).sin();
                    vph += vib_inc;
                    if vph >= 1.0 {
                        vph -= 1.0;
                    }
                }
                let f = mtof(semi);
                dt = f / SRF;
                let t = n.t + k as f64 / SR as f64;
                let fc = tone.cut * (f / 220.0).powf(tone.key) * (1.0 + tone.env_amt * fenv) * cut_mod(t);
                svf.set(fc.min(16_000.0), tone.q);
            }
            if k < gate {
                if attacking {
                    env += a_inc;
                    if env >= 1.0 {
                        env = 1.0;
                        attacking = false;
                    }
                } else {
                    env = tone.s + (env - tone.s) * d_mul;
                }
            } else {
                env *= r_mul;
            }
            fenv *= f_mul;
            p1 += dt;
            if p1 >= 1.0 {
                p1 -= 1.0;
            }
            let mut o = 0.0;
            if tone.saw > 0.0 {
                o += tone.saw * saw(p1, dt);
            }
            if tone.sqr > 0.0 {
                o += tone.sqr * pulse(p1, dt, tone.pw);
            }
            if tone.saw2 > 0.0 {
                let dt2 = dt * det;
                p2 += dt2;
                if p2 >= 1.0 {
                    p2 -= 1.0;
                }
                o += tone.saw2 * saw(p2, dt2);
            }
            if tone.hp > 0.0 {
                o = hp2.hp(hp1.hp(o));
            }
            let mut y = svf.lp(o);
            if tone.sine > 0.0 {
                y += tone.sine * sin01(p1);
            }
            let y = y * env * (1.0 - bus.duck * mix.sc(i)).max(0.0);
            let (l, r) = (y * gl, y * gr);
            mix.l[i] += l;
            mix.r[i] += r;
            let m = 0.5 * (l + r);
            mix.rev[i] += m * bus.rev;
            mix.dly[i] += m * bus.dly;
            k += 1;
        }
    }
}

const PAD_V: usize = 5;

/// One supersaw pad chord (four notes x five detuned saws spread across the stereo field).
#[allow(clippy::too_many_arguments)]
fn pad(mix: &mut Mix, rng: &mut Rng, t0: f64, t1: f64, notes: &[i32], vel: f32, attack: f32, detune: f32, cut: &dyn Fn(f64) -> f32) {
    const NV: usize = 4 * PAD_V;
    let release = 0.3f32;
    let s0 = smp(t0);
    let gate = smp(t1).saturating_sub(s0);
    let rlen = (release * 4.0 * SRF) as usize;
    let end = (s0 + gate + rlen).min(mix.len);
    let mut ph = [0.0f32; NV];
    let mut inc = [0.0f32; NV];
    let mut wl = [0.0f32; NV];
    let mut wr = [0.0f32; NV];
    for (ni, &m) in notes.iter().take(4).enumerate() {
        let f = mtof(m as f32);
        for v in 0..PAD_V {
            let j = ni * PAD_V + v;
            let c = (v as f32 - 2.0) * 0.5 * detune + rng.range(-1.5, 1.5);
            inc[j] = f * (c / 1200.0).exp2() / SRF;
            ph[j] = rng.f32();
            let pan = (((v + ni * 2) % PAD_V) as f32 / (PAD_V - 1) as f32 * 2.0 - 1.0) * 0.85;
            let (l, r) = pan2(1.0, pan);
            wl[j] = l;
            wr[j] = r;
        }
    }
    let mut fl = Svf::new(cut(t0), 0.8);
    let mut fr = Svf::new(cut(t0), 0.8);
    let a_inc = 1.0 / (attack * SRF).max(1.0);
    let r_mul = decay_coef(release);
    let mut env = 0.0f32;
    let g = vel * 0.05;
    for (k, i) in (s0..end).enumerate() {
        if k % 32 == 0 {
            let fc = cut(t0 + k as f64 / SR as f64);
            fl.set(fc, 0.8);
            fr.set(fc, 0.8);
        }
        if k < gate {
            env = (env + a_inc).min(1.0);
        } else {
            env *= r_mul;
        }
        let (mut l, mut r) = (0.0f32, 0.0f32);
        for j in 0..NV {
            let mut p = ph[j] + inc[j];
            if p >= 1.0 {
                p -= 1.0;
            }
            ph[j] = p;
            let s = saw(p, inc[j]);
            l += s * wl[j];
            r += s * wr[j];
        }
        let left = (gate + rlen - k) as f32 * (1.0 / 4096.0);
        let d = env * g * (1.0 - 0.5 * mix.sc(i)) * left.min(1.0);
        let (l, r) = (fl.lp(l) * d, fr.lp(r) * d);
        mix.l[i] += l;
        mix.r[i] += r;
        mix.rev[i] += 0.3 * (l + r);
    }
}

// ---------------------------------------------------------------------------------------------
// Effects and master
// ---------------------------------------------------------------------------------------------

/// Tempo-synced ping-pong delay on the delay send; its output also feeds the reverb a little.
fn delay(mix: &mut Mix, secs: f64, fb: f32) {
    let d = smp(secs).max(1);
    let mut bl = vec![0.0f32; d];
    let mut br = vec![0.0f32; d];
    let mut hp = Svf::new(320.0, 0.7);
    let mut lpl = Svf::new(3800.0, 0.6);
    let mut lpr = Svf::new(3800.0, 0.6);
    let mut w = 0usize;
    for i in 0..mix.len {
        let x = hp.hp(mix.dly[i]);
        let yl = bl[w];
        let yr = br[w];
        bl[w] = x + fb * lpl.lp(yr);
        br[w] = fb * lpr.lp(yl) + 0.15 * x;
        w += 1;
        if w == d {
            w = 0;
        }
        mix.l[i] += 0.55 * yl;
        mix.r[i] += 0.55 * yr;
        mix.rev[i] += 0.12 * (yl + yr);
    }
}

/// 8-line feedback delay network reverb with Hadamard mixing and HF damping.
fn reverb(mix: &mut Mix, t60: f32, wet: f32) {
    const LENS: [usize; 8] = [1031, 1327, 1523, 1789, 2053, 2311, 2633, 2909];
    const AP: [usize; 2] = [142, 379];
    let mut lines: Vec<Vec<f32>> = LENS.iter().map(|&l| vec![0.0; l]).collect();
    let mut pos = [0usize; 8];
    let mut gain = [0.0f32; 8];
    for (g, &l) in gain.iter_mut().zip(LENS.iter()) {
        *g = 10f32.powf(-3.0 * l as f32 / (t60 * SRF));
    }
    let mut damp = [0.0f32; 8];
    let dc = 0.32f32;
    let mut aps: Vec<Vec<f32>> = AP.iter().map(|&l| vec![0.0; l]).collect();
    let mut ap_pos = [0usize; 2];
    let pre = smp(0.018);
    let mut pre_buf = vec![0.0f32; pre.max(1)];
    let mut pre_pos = 0usize;
    let mut hp = Svf::new(220.0, 0.7);
    let norm = 1.0 / 8f32.sqrt();
    for i in 0..mix.len {
        // pre-delay, low cut, two allpass diffusers
        let mut x = pre_buf[pre_pos];
        pre_buf[pre_pos] = hp.hp(mix.rev[i]);
        pre_pos += 1;
        if pre_pos == pre_buf.len() {
            pre_pos = 0;
        }
        for a in 0..2 {
            let buf = &mut aps[a];
            let p = ap_pos[a];
            let z = buf[p];
            let v = x + 0.6 * z;
            buf[p] = v;
            x = z - 0.6 * v;
            ap_pos[a] = if p + 1 == buf.len() { 0 } else { p + 1 };
        }
        let mut s = [0.0f32; 8];
        for j in 0..8 {
            let y = lines[j][pos[j]];
            damp[j] = y * (1.0 - dc) + damp[j] * dc;
            s[j] = damp[j];
        }
        let (ol, or) = (s[0] - s[2] + s[4] - s[6], s[1] - s[3] + s[5] - s[7]);
        // fast Walsh-Hadamard transform
        let mut h = 1;
        while h < 8 {
            let mut a = 0;
            while a < 8 {
                for j in a..a + h {
                    let (u, v) = (s[j], s[j + h]);
                    s[j] = u + v;
                    s[j + h] = u - v;
                }
                a += 2 * h;
            }
            h *= 2;
        }
        for j in 0..8 {
            let sign = if j & 1 == 0 { 1.0 } else { -1.0 };
            lines[j][pos[j]] = (s[j] * norm + x * sign * 0.5) * gain[j];
            pos[j] += 1;
            if pos[j] == LENS[j] {
                pos[j] = 0;
            }
        }
        mix.l[i] += ol * wet;
        mix.r[i] += or * wet;
    }
}

fn sidechain(n: usize, kicks: &[(usize, f32)]) -> Vec<f32> {
    let mut sc = vec![0.0f32; n];
    let len = (0.4 * SRF) as usize;
    let att = (0.004 * SRF) as usize;
    let k_mul = decay_coef(0.11);
    for &(s, v) in kicks {
        let mut e = 1.0f32;
        for k in 0..len {
            let a = if k < att { k as f32 / att as f32 } else { 1.0 };
            let i = (s + k) % n;
            let val = a * e * v;
            if val > sc[i] {
                sc[i] = val;
            }
            if k >= att {
                e *= k_mul;
            }
        }
    }
    sc
}

// ---------------------------------------------------------------------------------------------
// Arrangement
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Bar {
    part: Part,
    idx: u32,
    len: u32,
    occ: u32,
}

struct Song<'a> {
    st: &'a Style,
    bars: Vec<Bar>,
    bar_s: f64,
    step: f64,
    lfo_period: f64,
}

impl Song<'_> {
    fn pos(&self, bar: usize, step: f64) -> f64 {
        (bar as f64 * 16.0 + step) * self.step
    }
    fn locate(&self, t: f64) -> (Bar, f32) {
        let bf = t / self.bar_s;
        let fl = bf.floor();
        let b = (fl as i64).rem_euclid(self.bars.len() as i64) as usize;
        (self.bars[b], (bf - fl) as f32)
    }
    fn chord(&self, b: usize) -> Chord {
        let bar = self.bars[b];
        let prog = if bar.part == Part::Chorus { &self.st.chorus } else { &self.st.verse };
        prog[(bar.idx % 4) as usize]
    }
    fn lfo(&self, t: f64) -> f32 {
        1.0 + 0.22 * ((t / self.lfo_period) as f32 * TAU).sin()
    }
    fn pad_cut(&self, t: f64) -> f32 {
        let (bar, frac) = self.locate(t);
        let x = (bar.idx as f32 + frac) / bar.len as f32;
        let c = self.st.pad_cut;
        match bar.part {
            Part::Intro => 420.0 + 1200.0 * x * x,
            Part::Verse => 1700.0 * self.lfo(t),
            Part::Build => 1500.0 + (c * 1.6 - 1500.0) * x * x,
            Part::Chorus => c * self.lfo(t),
            Part::Break => {
                if bar.idx + 1 == bar.len {
                    700.0
                } else {
                    1000.0 + 1800.0 * x
                }
            }
            Part::Outro => 2400.0 - 1980.0 * x.sqrt(),
        }
    }
    fn arp_cut(&self, t: f64) -> f32 {
        let (bar, frac) = self.locate(t);
        let x = (bar.idx as f32 + frac) / bar.len as f32;
        match bar.part {
            Part::Intro => 0.4 + 0.5 * x,
            Part::Verse => 0.9 + 0.2 * self.lfo(t),
            Part::Build => 1.0 + 2.0 * x * x,
            Part::Chorus => 1.4,
            Part::Break => 0.6,
            Part::Outro => 1.0 - 0.6 * x,
        }
    }
}

fn render(st: &Style) -> Vec<f32> {
    let bar_s = 240.0 / st.bpm;
    let mut bars = Vec::new();
    for (pi, &(part, len)) in st.form.iter().enumerate() {
        let occ = st.form[..pi].iter().filter(|f| f.0 == part).count() as u32;
        for idx in 0..len {
            bars.push(Bar { part, idx, len, occ });
        }
    }
    let nb = bars.len();
    let total = nb as f64 * bar_s;
    let song = Song { st, bars, bar_s, step: bar_s / 16.0, lfo_period: total / ((nb as f64 / 8.0).round().max(1.0)) };
    let n = smp(total);
    let len = n + smp(TAIL_SECS);
    let mut rng = Rng::new(st.seed);
    let mut nz = Noise((rng.next_u64() as u32) | 1);

    // ---- drum schedule (kicks first: they drive the sidechain) ----
    let mut kicks: Vec<(usize, f32)> = Vec::new();
    let mut snares: Vec<(usize, f32, f32)> = Vec::new();
    let mut hats: Vec<(usize, f32, bool)> = Vec::new();
    let mut toms: Vec<(usize, f32, f32)> = Vec::new();
    let mut crashes: Vec<(usize, f32)> = Vec::new();
    let mut booms: Vec<usize> = Vec::new();
    let mut risers: Vec<(usize, usize, f32, f32, f32, bool)> = Vec::new();
    let at = |b: usize, s: f64| smp(song.pos(b, s));
    for b in 0..nb {
        let bar = song.bars[b];
        let next = song.bars[(b + 1) % nb];
        let last = bar.idx + 1 == bar.len;
        let (mut drums, mut hats_only, mut chorus) = (false, false, false);
        match bar.part {
            Part::Verse => drums = true,
            Part::Chorus => {
                drums = true;
                chorus = true;
            }
            Part::Outro => {
                if bar.idx < bar.len / 2 {
                    drums = true;
                } else if bar.idx + 2 < bar.len {
                    hats_only = true;
                }
            }
            _ => {}
        }
        let fill = drums && last && next.part != bar.part;
        if drums {
            let kick_steps: &[(u32, f32)] = match (st.groove, chorus) {
                (Groove::Half, false) => &[(0, 1.0), (8, 0.95)],
                (Groove::Half, true) => &[(0, 1.0), (8, 0.95), (10, 0.7)],
                _ => &[(0, 1.0), (4, 0.95), (8, 1.0), (12, 0.95)],
            };
            for &(s, v) in kick_steps {
                kicks.push((at(b, s as f64), v));
            }
            for s in [4u32, 12] {
                if !(fill && s == 12) {
                    snares.push((at(b, s as f64), if s == 12 { 1.0 } else { 0.95 }, 1.0));
                }
            }
            for s in 0..16u32 {
                if fill && s >= 12 {
                    continue;
                }
                let hum = rng.range(0.9, 1.1);
                let (v, open) = match (st.groove, chorus) {
                    (Groove::Four, false) => (if s % 4 == 2 { 0.8 } else { 0.0 }, false),
                    (Groove::Four, true) => match s % 4 {
                        2 => (0.7, true),
                        0 => (0.5, false),
                        _ => (0.28, false),
                    },
                    (Groove::Half, false) => (if s % 2 == 0 { if s % 4 == 0 { 0.7 } else { 0.45 } } else { 0.0 }, false),
                    (Groove::Half, true) => {
                        if s == 14 {
                            (0.6, true)
                        } else {
                            ([0.6, 0.22, 0.42, 0.22][(s % 4) as usize], false)
                        }
                    }
                    (Groove::Drive, c) => {
                        if c && s % 4 == 2 {
                            (0.65, true)
                        } else {
                            ([0.42, 0.2, 0.75, 0.22][(s % 4) as usize], false)
                        }
                    }
                };
                if v > 0.0 {
                    hats.push((at(b, s as f64), v * hum, open));
                }
            }
            if fill {
                for (j, f) in [210.0f32, 175.0, 145.0, 118.0].iter().enumerate() {
                    toms.push((at(b, 12.0 + j as f64), *f, 0.4 + 0.06 * j as f32));
                }
            }
        } else if hats_only {
            for s in [2u32, 6, 10, 14] {
                hats.push((at(b, s as f64), 0.6 * rng.range(0.9, 1.1), false));
            }
        }
        if bar.part == Part::Build {
            let x = bar.idx as f32 / bar.len as f32;
            let rate = if x < 0.5 { 4 } else if x < 0.75 { 8 } else { 16 };
            let stop = if last { 12 } else { 16 };
            for s in (0..stop).step_by(16 / rate) {
                let p = (bar.idx as f32 * 16.0 + s as f32) / (bar.len as f32 * 16.0);
                snares.push((at(b, s as f64), 0.3 + 0.6 * p * p, if rate == 4 { 1.0 } else { 0.35 }));
            }
            for s in (0..stop).step_by(4) {
                kicks.push((at(b, s as f64), 0.95));
            }
            let hstep = if x < 0.5 { 2 } else { 1 };
            for s in (0..stop).step_by(hstep) {
                hats.push((at(b, s as f64), if s % 2 == 0 { 0.55 } else { 0.3 }, false));
            }
            if bar.idx == 0 {
                let s1 = at(b + bar.len as usize - 1, 12.0);
                risers.push((at(b, 0.0), s1, 350.0, 7000.0, 0.22, false));
            }
        }
        if bar.idx == 0 {
            match bar.part {
                Part::Chorus => {
                    crashes.push((at(b, 0.0), 0.3));
                    booms.push(at(b, 0.0));
                }
                Part::Verse | Part::Outro => crashes.push((at(b, 0.0), 0.18)),
                Part::Break => crashes.push((at(b, 0.0), 0.2)),
                _ => {}
            }
        }
        if last && (bar.part == Part::Intro || bar.part == Part::Break) {
            risers.push((at(b, 0.0), at(b, 16.0), 6000.0, 6000.0, 0.16, true));
        }
        if last && bar.part == Part::Break {
            risers.push((at(b, 0.0), at(b, 16.0), 300.0, 5000.0, 0.14, false));
        }
    }

    let mut mix = Mix { l: vec![0.0; len], r: vec![0.0; len], rev: vec![0.0; len], dly: vec![0.0; len], sc: sidechain(n, &kicks), n, len };

    for &(s, v) in &kicks {
        // tune the kick tail to the sub (same pitch or an octave apart) so they never beat
        let b = ((s as f64 / SR as f64 / bar_s) as usize).min(nb - 1);
        let mut f = mtof(song.chord(b)[0] as f32);
        while f >= st.kick_end * 1.6 {
            f *= 0.5;
        }
        while f < st.kick_end * 0.8 {
            f *= 2.0;
        }
        kick(&mut mix, s, v, st.kick_decay, f, &mut nz);
    }
    for &s in &booms {
        boom(&mut mix, s, 0.25);
    }
    for &(s, v, tail) in &snares {
        snare(&mut mix, s, v * 0.42, tail, st.clap, &mut nz);
    }
    for (j, &(s, v, open)) in hats.iter().enumerate() {
        let pan = if j % 2 == 0 { 0.18 } else { -0.12 };
        hat(&mut mix, s, v * if open { 0.11 } else { 0.13 }, open, pan, &mut nz);
    }
    for (j, &(s, f, v)) in toms.iter().enumerate() {
        tom(&mut mix, s, f, v, 0.35 - 0.23 * (j % 4) as f32, &mut nz);
    }
    for &(s, v) in &crashes {
        crash(&mut mix, s, v * 0.5, &mut nz);
    }
    for &(s0, s1, a, z, v, high) in &risers {
        riser(&mut mix, s0, s1, a, z, v, high, &mut nz);
    }

    // ---- pad ----
    let pad_cut = |t: f64| song.pad_cut(t);
    for b in 0..nb {
        let bar = song.bars[b];
        let (lvl, att) = match bar.part {
            Part::Intro => (0.85, 0.6),
            Part::Verse => (1.0, 0.25),
            Part::Build => (0.6 + 0.35 * bar.idx as f32 / bar.len as f32, 0.2),
            Part::Chorus => (0.95, 0.06),
            Part::Break => (if bar.idx + 1 == bar.len { 0.7 } else { 1.0 }, 0.5),
            Part::Outro => (0.75, 0.3),
        };
        let c = song.chord(b);
        pad(&mut mix, &mut rng, song.pos(b, 0.0), song.pos(b, 16.0), &c[1..], lvl, att, st.pad_detune, &pad_cut);
    }

    // ---- bass: a high-passed pluck layer plays the pattern, a sine sub holds the root ----
    // (keeping the 30-160 Hz band steady between kicks so the kick stays the low-band onset)
    let mut bass = Vec::new();
    let mut sub = Vec::new();
    for b in 0..nb {
        let bar = song.bars[b];
        let root = song.chord(b)[0];
        let last = bar.idx + 1 == bar.len;
        let mut sub_v = 0.0;
        let mut sub_len = 16.0;
        match bar.part {
            Part::Verse | Part::Chorus => {
                let v = if bar.part == Part::Chorus { 1.0 } else { 0.75 };
                sub_v = v;
                for (s, &o) in st.bass_pat.iter().enumerate() {
                    if o != R {
                        let acc = if s % 4 == 0 { 1.0 } else { 0.85 };
                        bass.push(Note { t: song.pos(b, s as f64), dur: st.bass_gate * song.step, midi: (root + o) as f32, vel: v * acc, pan: 0.0 });
                    }
                }
            }
            Part::Build => {
                let stop = if last { 12 } else { 16 };
                sub_v = 0.85;
                sub_len = stop as f64;
                for s in (0..stop).step_by(2) {
                    bass.push(Note { t: song.pos(b, s as f64), dur: 2.0 * song.step, midi: root as f32, vel: 0.85, pan: 0.0 });
                }
            }
            Part::Outro if bar.idx < bar.len / 2 + 1 => {
                sub_v = 0.7;
                for s in (0..16).step_by(2) {
                    bass.push(Note { t: song.pos(b, s as f64), dur: 2.0 * song.step, midi: root as f32, vel: 0.7, pan: 0.0 });
                }
            }
            Part::Break if !last && bar.idx >= 2 => {
                sub_v = 0.5;
                sub_len = 15.0;
            }
            _ => {}
        }
        if sub_v > 0.0 {
            sub.push(Note { t: song.pos(b, 0.0), dur: sub_len * song.step, midi: root as f32, vel: sub_v, pan: 0.0 });
        }
    }
    let sub_tone = Tone { sine: 1.0, a: 0.008, d: 0.5, s: 1.0, r: 0.08, legato_f: 0.0, ..TONE0 };
    lane(&mut mix, &sub, &sub_tone, &Bus { gain: 0.25 * st.bass.sine, duck: 0.5, rev: 0.0, dly: 0.0 }, &|_| 1.0);
    let pluck = Tone { sine: 0.0, hp: 220.0, ..st.bass };
    lane(&mut mix, &bass, &pluck, &Bus { gain: 0.36, duck: 0.6, rev: 0.0, dly: 0.0 }, &|_| 1.0);

    // ---- arpeggio ----
    let mut arp = Vec::new();
    for b in 0..nb {
        let bar = song.bars[b];
        let last = bar.idx + 1 == bar.len;
        let lvl = match bar.part {
            Part::Intro => {
                if bar.idx >= st.arp_from {
                    0.35 + 0.3 * (bar.idx - st.arp_from) as f32 / (bar.len - st.arp_from).max(1) as f32
                } else {
                    0.0
                }
            }
            Part::Verse => 1.25,
            Part::Build => {
                if last {
                    0.0
                } else {
                    0.9
                }
            }
            Part::Chorus => 0.7,
            Part::Break => {
                if bar.idx >= bar.len / 2 && !last {
                    0.4
                } else {
                    0.0
                }
            }
            Part::Outro => 0.8 - 0.3 * bar.idx as f32 / bar.len as f32,
        };
        if lvl <= 0.0 {
            continue;
        }
        let c = song.chord(b);
        let mut tones = [c[1], c[2], c[3], c[4], c[1] + 12, c[2] + 12];
        for t in tones.iter_mut() {
            *t += st.arp_oct;
        }
        let per = 16 / st.arp_rate;
        for j in 0..per {
            let s = (j * st.arp_rate) as f64;
            let idx = st.arp_pat[j as usize % st.arp_pat.len()];
            let acc = if j % 4 == 0 { 1.0 } else { 0.8 };
            let pan = if j % 2 == 0 { 0.3 } else { -0.3 };
            arp.push(Note { t: song.pos(b, s), dur: 0.5 * st.arp_rate as f64 * song.step, midi: tones[idx] as f32, vel: lvl * acc, pan });
        }
    }
    let arp_cut = |t: f64| song.arp_cut(t);
    lane(&mut mix, &arp, &st.arp, &Bus { gain: 0.14, duck: 0.35, rev: 0.25, dly: 0.45 }, &arp_cut);

    // ---- lead ----
    let mut lead = Vec::new();
    let mut lead_low = Vec::new();
    let mut sus = Vec::new();
    for b in 0..nb {
        let bar = song.bars[b];
        match bar.part {
            Part::Chorus => {
                let mb = bar.idx % 8;
                for &(_, s, l, m) in st.motif.iter().filter(|x| x.0 == mb) {
                    let full = l as f64 * song.step;
                    let dur = if st.legato { full } else { full - 0.035 };
                    let nt = Note { t: song.pos(b, s as f64), dur, midi: m as f32, vel: 1.0, pan: 0.0 };
                    lead.push(nt);
                    if bar.occ > 0 {
                        lead_low.push(Note { midi: nt.midi - 12.0, vel: 0.55, ..nt });
                    }
                }
            }
            Part::Break => {
                for &(_, l, m) in st.sustain.iter().filter(|x| x.0 == bar.idx) {
                    sus.push(Note { t: song.pos(b, 0.0) + 0.02, dur: l as f64 * bar_s - 0.15, midi: m as f32, vel: 0.9, pan: 0.0 });
                }
            }
            _ => {}
        }
    }
    let lead_bus = Bus { gain: 0.21, duck: 0.12, rev: 0.25, dly: 0.28 };
    lane(&mut mix, &lead, &st.lead, &lead_bus, &|_| 1.0);
    lane(&mut mix, &lead_low, &Tone { vib: 0.0, ..st.lead }, &lead_bus, &|_| 0.7);
    let sus_tone = Tone { a: 0.45, r: 0.6, s: 0.9, vib: st.lead.vib * 1.4, vib_delay: 0.5, env_amt: 0.3, ..st.lead };
    lane(&mut mix, &sus, &sus_tone, &Bus { gain: 0.2, duck: 0.0, rev: 0.4, dly: 0.3 }, &|_| 1.0);

    // ---- effects, loop fold, master ----
    delay(&mut mix, st.dly_steps * song.step, st.dly_fb);
    reverb(&mut mix, st.rev_t60, 0.3);
    let Mix { mut l, mut r, .. } = mix;
    for i in 0..len - n {
        l[i] += l[n + i];
        r[i] += r[n + i];
    }
    l.truncate(n);
    r.truncate(n);

    let o = master(&song, &mut l, &mut r);
    o
}

/// High-pass at 25 Hz, normalise the choruses to the target RMS, then a look-ahead peak limiter.
/// Stateful stages are warmed up on the end of the loop so the loop point stays seamless.
fn master(song: &Song, l: &mut [f32], r: &mut [f32]) -> Vec<f32> {
    let n = l.len();
    let warm = n.min(SR as usize);
    for ch in [&mut *l, &mut *r] {
        let mut a = Svf::new(25.0, 0.707);
        for &x in &ch[n - warm..] {
            a.hp(x);
        }
        for x in ch.iter_mut() {
            *x = a.hp(*x);
        }
    }
    // chorus loudness
    let (mut acc, mut cnt) = (0.0f64, 0usize);
    for (b, bar) in song.bars.iter().enumerate() {
        if bar.part == Part::Chorus {
            let (s0, s1) = (smp(song.pos(b, 0.0)).min(n), smp(song.pos(b, 16.0)).min(n));
            for i in s0..s1 {
                acc += (l[i] as f64).powi(2) + (r[i] as f64).powi(2);
            }
            cnt += 2 * (s1 - s0);
        }
    }
    let rms = (acc / cnt.max(1) as f64).sqrt() as f32;
    let gain = TARGET_RMS / rms.max(1e-6);
    // gain needed per sample, then backward (attack) and forward (release) smoothing
    let mut g: Vec<f32> = (0..n)
        .map(|i| {
            let p = l[i].abs().max(r[i].abs()) * gain;
            if p > CEIL {
                CEIL / p
            } else {
                1.0
            }
        })
        .collect();
    let att = 1.0 - decay_coef(0.0015);
    let rel = 1.0 - decay_coef(0.08);
    let mut e = 1.0f32;
    for i in (0..warm).rev() {
        e = g[i].min(e + (1.0 - e) * att);
    }
    for i in (0..n).rev() {
        e = g[i].min(e + (1.0 - e) * att);
        g[i] = e;
    }
    let mut f = 1.0f32;
    for &x in &g[n - warm..] {
        f = x.min(f + (1.0 - f) * rel);
    }
    let mut out = Vec::with_capacity(2 * n);
    for i in 0..n {
        f = g[i].min(f + (1.0 - f) * rel);
        let k = gain * f;
        out.push((l[i] * k).clamp(-CEIL, CEIL));
        out.push((r[i] * k).clamp(-CEIL, CEIL));
    }
    out
}
