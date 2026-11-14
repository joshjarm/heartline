//! Breaks audio down into the features the level generator needs:
//! band energies, a 24-band spectrum, per-band onsets (kick / snare / hat-ish),
//! tempo, beats, bars, an intensity curve, sections, sustained phrases and silences.
//!
//! `analyze` works on a whole decoded track; `StreamAnalyzer` runs the same spectral
//! front-end incrementally for the live (system audio) mode.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

pub const N_SPEC: usize = 24;

/// Tempo search range.
const MIN_BPM: f32 = 55.0;
const MAX_BPM: f32 = 220.0;
/// A different metrical level must beat the autocorrelation pick by this factor to replace it.
const TEMPO_SWITCH_MARGIN: f32 = 1.1;
/// Tempi below this are doubled.
const MIN_PLAY_BPM: f32 = 72.0;
/// Added to every reported onset / beat time to cancel the STFT front-end's measured timing bias.
const ONSET_BIAS_S: f32 = 0.0038;
/// How much of the mid band's per-bin flux is removed from the low band before kick picking.
const KICK_BROADBAND: f32 = 1.0;
/// Half-width (in beats) of the local line fit that smooths beat times.
const BEAT_FIT_HALF: usize = 4;
/// Downbeat evidence weights: kick accent, harmonic change, backbeat (snare on the next beat).
const DOWNBEAT_W: [f32; 3] = [1.0, 0.8, 0.5];
/// Intensity mix: loudness, onset density, spectral fullness, low end (all z-scored per song).
const INTENSITY_W: [f32; 4] = [0.4, 0.25, 0.2, 0.15];
/// Section labels by duration-weighted quantile of section energy within the song.
const SECTION_CALM_Q: f32 = 0.22;
const SECTION_INTENSE_Q: f32 = 0.62;
const SECTION_HYST: f32 = 0.05;
/// An Intense section is a Drop when its energy jumps this many section-sd above the previous one.
const DROP_JUMP_SD: f32 = 0.7;
/// ... or when its energy quantile within the song jumps by at least this much.
const DROP_JUMP_Q: f32 = 0.4;
/// beat_conf = smoothstep of (0.7 * grid fit + 0.3 * periodicity) between these.
const BEAT_CONF_LO: f32 = 0.3;
const BEAT_CONF_HI: f32 = 0.75;
/// Live beat confidence ramps from 0 to 1 as the envelope's normalised autocorrelation at the beat goes LO..HI.
const LIVE_CONF_LO: f32 = 0.1;
const LIVE_CONF_HI: f32 = 0.4;

#[derive(Clone, Copy, Debug)]
pub struct Feat {
    pub rms: f32,
    pub low: f32,
    pub mid: f32,
    pub high: f32,
    pub spec: [f32; N_SPEC],
}

impl Default for Feat {
    fn default() -> Self {
        Feat { rms: 0.0, low: 0.0, mid: 0.0, high: 0.0, spec: [0.0; N_SPEC] }
    }
}

impl Feat {
    pub fn lerp(a: &Feat, b: &Feat, t: f32) -> Feat {
        let l = |x: f32, y: f32| x + (y - x) * t;
        let mut spec = [0.0; N_SPEC];
        for i in 0..N_SPEC {
            spec[i] = l(a.spec[i], b.spec[i]);
        }
        Feat { rms: l(a.rms, b.rms), low: l(a.low, b.low), mid: l(a.mid, b.mid), high: l(a.high, b.high), spec }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
    Low = 0,
    Mid = 1,
    High = 2,
}

#[derive(Clone, Copy, Debug)]
pub struct Onset {
    pub t: f32,
    pub band: Band,
    pub strength: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    Calm,
    Normal,
    Intense,
    Drop,
}

#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub t0: f32,
    pub t1: f32,
    pub intensity: f32,
    pub kind: SectionKind,
}

pub struct Analysis {
    pub duration: f32,
    pub hop_dt: f32,
    pub feats: Vec<Feat>,
    pub intensity: Vec<f32>,
    pub onsets: Vec<Onset>,
    pub bpm: f32,
    pub beats: Vec<f32>,
    /// Index into `beats` of the first downbeat (bars are every 4 beats from here).
    pub downbeat: usize,
    pub sections: Vec<Section>,
    pub phrases: Vec<(f32, f32)>,
    pub silences: Vec<(f32, f32)>,
    pub seed: u64,
    /// 0..1 confidence that the beat grid is musically meaningful (near 0 for free-time pieces).
    pub beat_conf: f32,
    /// 0..1 confidence in the bar phase (`downbeat`): margin of the winning phase over the runner-up.
    pub downbeat_conf: f32,
}

impl Analysis {
    pub fn intensity_at(&self, t: f32) -> f32 {
        if self.intensity.is_empty() {
            return 0.0;
        }
        let i = ((t / self.hop_dt) as isize).clamp(0, self.intensity.len() as isize - 1) as usize;
        self.intensity[i]
    }
    pub fn in_silence(&self, t: f32) -> bool {
        self.silences.iter().any(|&(a, b)| t >= a && t < b)
    }
    pub fn section_at(&self, t: f32) -> Option<&Section> {
        self.sections.iter().find(|s| t >= s.t0 && t < s.t1)
    }
}

// ---------------------------------------------------------------------------------------------
// Spectral front-end shared by offline and streaming analysis
// ---------------------------------------------------------------------------------------------

struct Raw {
    rms: f32,
    flux: [f32; 3],
    /// Low-band flux with the broadband (snare / hat / crash) part removed: drives kick onsets.
    kick: f32,
    energy: [f32; 3],
    spec: [f32; N_SPEC],
}

pub struct Spectral {
    pub n: usize,
    pub hop: usize,
    pub sr: f32,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    norm: f32,
    buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    logmag: Vec<f32>,
    prev: Vec<f32>,
    has_prev: bool,
    band_bins: [(usize, usize); 3],
    spec_bins: [(usize, usize); N_SPEC],
}

impl Spectral {
    pub fn new(sr: u32) -> Self {
        let n = if sr > 60_000 { 4096 } else if sr < 30_000 { 1024 } else { 2048 };
        let hop = n / 4;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n);
        let window: Vec<f32> = (0..n)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos())
            .collect();
        let norm = 2.0 / window.iter().sum::<f32>();
        let scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let half = n / 2;
        let srf = sr as f32;
        let bin = |hz: f32| -> usize { ((hz * n as f32 / srf).round() as usize).clamp(1, half - 1) };
        let rng = |a: f32, b: f32| -> (usize, usize) {
            let lo = bin(a);
            let hi = bin(b).max(lo + 1).min(half);
            (lo, hi)
        };
        let band_bins = [rng(30.0, 160.0), rng(300.0, 3000.0), rng(5000.0, 16000.0)];
        let mut spec_bins = [(1usize, 2usize); N_SPEC];
        let (f0, f1) = (40.0f32, 16000.0f32.min(srf * 0.48));
        for (i, sb) in spec_bins.iter_mut().enumerate() {
            let a = f0 * (f1 / f0).powf(i as f32 / N_SPEC as f32);
            let b = f0 * (f1 / f0).powf((i + 1) as f32 / N_SPEC as f32);
            *sb = rng(a, b);
        }
        Spectral {
            n,
            hop,
            sr: srf,
            fft,
            window,
            norm,
            buf: vec![Complex::new(0.0, 0.0); n],
            scratch,
            logmag: vec![0.0; half],
            prev: vec![0.0; half],
            has_prev: false,
            band_bins,
            spec_bins,
        }
    }

    pub fn hop_dt(&self) -> f32 {
        self.hop as f32 / self.sr
    }

    fn process(&mut self, frame: &[f32]) -> Raw {
        debug_assert_eq!(frame.len(), self.n);
        let mut sq = 0.0f32;
        for i in 0..self.n {
            let x = frame[i];
            sq += x * x;
            self.buf[i] = Complex::new(x * self.window[i], 0.0);
        }
        let rms = (sq / self.n as f32).sqrt();
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        let half = self.n / 2;
        std::mem::swap(&mut self.prev, &mut self.logmag);
        let mut energy = [0.0f32; 3];
        for k in 0..half {
            let m = self.buf[k].norm() * self.norm;
            self.logmag[k] = (1.0 + 300.0 * m).ln();
        }
        for (b, &(lo, hi)) in self.band_bins.iter().enumerate() {
            let mut e = 0.0;
            for k in lo..hi {
                let m = self.buf[k].norm() * self.norm;
                e += m * m;
            }
            energy[b] = (e / (hi - lo) as f32).sqrt();
        }
        let mut flux = [0.0f32; 3];
        let mut kick = 0.0f32;
        if self.has_prev {
            let mut per_bin = [0.0f32; 3];
            for (b, &(lo, hi)) in self.band_bins.iter().enumerate() {
                let mut f = 0.0;
                for k in lo..hi {
                    let a = self.prev[k - 1];
                    let c = self.prev[(k + 1).min(half - 1)];
                    let reference = self.prev[k].max(a).max(c);
                    let d = self.logmag[k] - reference;
                    if d > 0.0 {
                        f += d;
                    }
                }
                flux[b] = f / ((hi - lo) as f32).sqrt();
                per_bin[b] = f / (hi - lo) as f32;
            }
            let (lo, hi) = self.band_bins[0];
            kick = (per_bin[0] - KICK_BROADBAND * per_bin[1]).max(0.0) * ((hi - lo) as f32).sqrt();
        }
        self.has_prev = true;
        let mut spec = [0.0f32; N_SPEC];
        for (i, &(lo, hi)) in self.spec_bins.iter().enumerate() {
            let mut s = 0.0;
            for k in lo..hi {
                s += self.logmag[k];
            }
            spec[i] = s / (hi - lo) as f32;
        }
        Raw { rms, flux, kick, energy, spec }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn percentile(v: &[f32], p: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let step = (v.len() / 200_000).max(1);
    let mut s: Vec<f32> = v.iter().step_by(step).copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return 0.0;
    }
    let k = (((s.len() - 1) as f32) * p.clamp(0.0, 1.0)) as usize;
    s.select_nth_unstable_by(k, |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[k]
}

/// Centered moving average with half-width `half` frames.
fn smooth(x: &[f32], half: usize) -> Vec<f32> {
    let n = x.len();
    if n == 0 || half == 0 {
        return x.to_vec();
    }
    let mut pre = Vec::with_capacity(n + 1);
    pre.push(0.0f64);
    for &v in x {
        pre.push(pre.last().unwrap() + v as f64);
    }
    (0..n)
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + half + 1).min(n);
            ((pre[b] - pre[a]) / (b - a) as f64) as f32
        })
        .collect()
}

fn mean_std(x: &[f32]) -> (f32, f32) {
    if x.is_empty() {
        return (0.0, 0.0);
    }
    let m = x.iter().sum::<f32>() / x.len() as f32;
    let v = x.iter().map(|a| (a - m) * (a - m)).sum::<f32>() / x.len() as f32;
    (m, v.sqrt())
}

struct PickParams {
    min_gap: f32,
    k_mean: f32,
    delta: f32,
}

/// Sub-frame position of a local maximum (parabolic interpolation), in frames, within +-0.5.
fn peak_offset(env: &[f32], i: usize) -> f32 {
    if i == 0 || i + 1 >= env.len() {
        return 0.0;
    }
    let (a, b, c) = (env[i - 1], env[i], env[i + 1]);
    let d = a - 2.0 * b + c;
    if d.abs() < 1e-12 {
        return 0.0;
    }
    (0.5 * (a - c) / d).clamp(-0.5, 0.5)
}

fn pick_onsets(env: &[f32], rms: &[f32], hop_dt: f32, band: Band, p: &PickParams) -> Vec<Onset> {
    let n = env.len();
    if n < 16 {
        return vec![];
    }
    let mut maxima = Vec::new();
    for i in 1..n - 1 {
        if env[i] > env[i - 1] && env[i] >= env[i + 1] && env[i] > 1e-4 {
            maxima.push(env[i]);
        }
    }
    let reference = percentile(&maxima, 0.90);
    if reference < 1e-3 {
        return vec![];
    }
    let e: Vec<f32> = env.iter().map(|v| v / reference).collect();
    let w = ((0.45 / hop_dt) as usize).max(4);
    let local = smooth(&e, w);
    let m = ((0.035 / hop_dt).round() as usize).max(2);
    let gap = ((p.min_gap / hop_dt).round() as usize).max(1);
    let mut out: Vec<(usize, f32)> = Vec::new();
    for i in m..n - m {
        let v = e[i];
        if v < p.k_mean * local[i] + p.delta || rms[i] < 0.0025 {
            continue;
        }
        let mut is_max = true;
        for j in i - m..=i + m {
            if e[j] > v {
                is_max = false;
                break;
            }
        }
        if !is_max {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if i - last.0 < gap {
                if v > last.1 {
                    *last = (i, v);
                }
                continue;
            }
        }
        out.push((i, v));
    }
    out.into_iter()
        .map(|(i, v)| Onset { t: (i as f32 + peak_offset(env, i)) * hop_dt + ONSET_BIAS_S, band, strength: (v / 1.3).min(1.0) })
        .collect()
}

/// Returns the beat period in frames.
fn estimate_tempo(o: &[f32], hop_dt: f32) -> Option<f32> {
    let n = o.len();
    let lo = ((60.0 / MAX_BPM) / hop_dt).floor() as usize;
    let hi = ((60.0 / MIN_BPM) / hop_dt).ceil() as usize;
    if n < hi * 6 || lo < 2 {
        return None;
    }
    let max_lag = hi * 4 + 4;
    let mut ac = vec![0.0f32; max_lag + 1];
    for (lag, slot) in ac.iter_mut().enumerate().skip(1) {
        let mut s = 0.0f64;
        for i in 0..n - lag {
            s += (o[i] * o[i + lag]) as f64;
        }
        *slot = (s / (n - lag) as f64) as f32;
    }
    let score = |lag: usize| -> f32 {
        let bpm = 60.0 / (lag as f32 * hop_dt);
        let oct = (bpm / 120.0).log2() / 0.85;
        let prior = (-0.5 * oct * oct).exp();
        (ac[lag] + 0.5 * ac[2 * lag] + 0.15 * (ac[2 * lag - 1] + ac[2 * lag + 1])) * prior
    };
    let mut best = lo;
    let mut best_s = f32::MIN;
    for lag in lo..=hi {
        let s = score(lag);
        if s > best_s {
            best_s = s;
            best = lag;
        }
    }
    if best_s <= 1e-6 {
        return None;
    }
    // Parabolic refinement.
    let mut period = best as f32;
    if best > lo && best < hi {
        let (a, b, c) = (score(best - 1), score(best), score(best + 1));
        let denom = a - 2.0 * b + c;
        if denom.abs() > 1e-9 {
            period += (0.5 * (a - c) / denom).clamp(-0.5, 0.5);
        }
    }
    Some(period)
}

/// Mean onset strength at the beat, half-beat, quarter-beat and third-beat positions of a beat grid.
fn grid_salience(o: &[f32], beats: &[usize]) -> [f32; 4] {
    let n = o.len() as isize;
    let at = |x: f32| -> f32 {
        let i = x.round() as isize;
        let mut m = 0.0f32;
        for j in (i - 1)..=(i + 1) {
            if j >= 0 && j < n {
                m = m.max(o[j as usize]);
            }
        }
        m
    };
    let mut acc = [0.0f32; 4];
    let mut cnt = 0.0f32;
    for w in beats.windows(2) {
        let (a, b) = (w[0] as f32, w[1] as f32);
        let d = b - a;
        acc[0] += at(a);
        acc[1] += at(a + d * 0.5);
        acc[2] += 0.5 * (at(a + d * 0.25) + at(a + d * 0.75));
        acc[3] += 0.5 * (at(a + d / 3.0) + at(a + d * 2.0 / 3.0));
        cnt += 1.0;
    }
    for v in acc.iter_mut() {
        *v /= cnt.max(1.0);
    }
    acc
}

/// Log-gaussian tempo preference centred on 120 BPM; `sigma` in octaves.
fn tempo_prior(bpm: f32, sigma: f32) -> f32 {
    let oct = (bpm / 120.0).log2() / sigma;
    (-0.5 * oct * oct).exp()
}

/// How well a beat grid explains the onset envelope as a metrical hierarchy. A binary grid wants
/// strong beats, then half-beats, then quarter-beats, and little at the thirds; a ternary grid
/// (6/8, shuffle) wants strong beats and thirds.
fn metrical_score(sal: &[f32; 4]) -> f32 {
    let [beat, half, quart, third] = *sal;
    let binary = beat + 0.6 * half + 0.3 * quart - 0.4 * third;
    let ternary = beat + 0.6 * third - 0.4 * half;
    binary.max(0.9 * ternary)
}

/// Resolves 4:3 and 3:2 tempo confusions. Autocorrelation alone cannot tell a beat from three
/// sixteenths (3+3+2 patterns) or a dotted quarter, but the grid tracked at the wrong period has a
/// tell-tale ternary subdivision and weaker beats. Octave (2:1) choices are left to the
/// autocorrelation prior. Returns the chosen period and its tracked beats.
fn choose_metrical_level(o: &[f32], p0: f32, hop_dt: f32) -> (f32, Vec<usize>) {
    let base = track_beats(o, p0);
    let bpm0 = 60.0 / (p0 * hop_dt);
    let s0 = metrical_score(&grid_salience(o, &base)) * tempo_prior(bpm0, 1.2);
    let mut best = (p0, base, s0 * TEMPO_SWITCH_MARGIN);
    for r in [4.0f32 / 3.0, 0.75, 2.0 / 3.0, 1.5] {
        let p = p0 * r;
        let bpm = 60.0 / (p * hop_dt);
        if !(MIN_BPM..=MAX_BPM).contains(&bpm) {
            continue;
        }
        let b = track_beats(o, p);
        if b.len() < 4 {
            continue;
        }
        let s = metrical_score(&grid_salience(o, &b)) * tempo_prior(bpm, 1.2);
        if s > best.2 {
            best = (p, b, s);
        }
    }
    (best.0, best.1)
}

/// Dynamic-programming beat tracker (after Ellis 2007). Returns beat frame indices.
fn track_beats(o: &[f32], period: f32) -> Vec<usize> {
    let n = o.len();
    if n < 8 || period < 2.0 {
        return vec![];
    }
    // Smooth the onset envelope with a narrow gaussian.
    let sigma = (period / 32.0).max(0.6);
    let half = (sigma * 3.0).ceil() as isize;
    let kernel: Vec<f32> = (-half..=half).map(|k| (-0.5 * (k as f32 / sigma).powi(2)).exp()).collect();
    let mut local = vec![0.0f32; n];
    for i in 0..n as isize {
        let mut s = 0.0;
        for (ki, k) in (-half..=half).enumerate() {
            let j = i + k;
            if j >= 0 && j < n as isize {
                s += o[j as usize] * kernel[ki];
            }
        }
        local[i as usize] = s;
    }
    let (_, sd) = mean_std(&local);
    if sd < 1e-9 {
        return vec![];
    }
    for v in local.iter_mut() {
        *v /= sd;
    }
    let tight = 100.0f32;
    let w_lo = (period / 2.0).round() as usize;
    let w_hi = (period * 2.0).round() as usize;
    let costs: Vec<f32> = (w_lo..=w_hi)
        .map(|d| -tight * ((d as f32 / period).ln()).powi(2))
        .collect();
    let max_local = local.iter().cloned().fold(0.0f32, f32::max);
    let mut cum = vec![0.0f32; n];
    let mut back = vec![-1i32; n];
    let mut first = true;
    for i in 0..n {
        let mut best = f32::MIN;
        let mut arg = -1i32;
        for (ci, d) in (w_lo..=w_hi).enumerate() {
            if d > i {
                break;
            }
            let s = cum[i - d] + costs[ci];
            if s > best {
                best = s;
                arg = (i - d) as i32;
            }
        }
        if arg >= 0 {
            cum[i] = local[i] + best;
        } else {
            cum[i] = local[i];
        }
        if first && local[i] < 0.01 * max_local {
            back[i] = -1;
        } else {
            back[i] = arg;
            first = false;
        }
    }
    // Pick the tail: last strong local maximum of the cumulative score.
    let mut maxima = Vec::new();
    for i in 1..n - 1 {
        if cum[i] > cum[i - 1] && cum[i] >= cum[i + 1] {
            maxima.push(cum[i]);
        }
    }
    if maxima.is_empty() {
        return vec![];
    }
    let med = percentile(&maxima, 0.5);
    let mut tail = n - 1;
    for i in (1..n - 1).rev() {
        if cum[i] > cum[i - 1] && cum[i] >= cum[i + 1] && cum[i] >= 0.5 * med {
            tail = i;
            break;
        }
    }
    let mut beats = vec![tail];
    let mut i = tail;
    while back[i] >= 0 {
        i = back[i] as usize;
        beats.push(i);
    }
    beats.reverse();
    beats
}

/// Turns DP beat frames into smooth, sub-frame beat times: each beat snaps to the parabolic peak of
/// the onset envelope when one is within a frame, then a local linear fit over +-4 beats removes
/// the frame-quantisation jitter while still following tempo drift.
fn refine_beats(o: &[f32], frames: &[usize], hop_dt: f32) -> Vec<f32> {
    let n = o.len();
    let raw: Vec<f32> = frames
        .iter()
        .map(|&f| {
            let mut best = f;
            for j in f.saturating_sub(1)..=(f + 1).min(n.saturating_sub(1)) {
                if o[j] > o[best] {
                    best = j;
                }
            }
            if best > 0 && best + 1 < n && o[best] >= o[best - 1] && o[best] >= o[best + 1] && o[best] > 0.0 {
                best as f32 + peak_offset(o, best)
            } else {
                f as f32
            }
        })
        .collect();
    let m = raw.len();
    (0..m)
        .map(|i| {
            let a = i.saturating_sub(BEAT_FIT_HALF);
            let z = (i + BEAT_FIT_HALF + 1).min(m);
            if z - a < 3 {
                return raw[i] * hop_dt + ONSET_BIAS_S;
            }
            // Least squares t = c0 + c1 * k, evaluated at k = i.
            let (mut sk, mut st, mut skk, mut skt) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
            let cnt = (z - a) as f64;
            for k in a..z {
                let (kf, t) = (k as f64, raw[k] as f64);
                sk += kf;
                st += t;
                skk += kf * kf;
                skt += kf * t;
            }
            let den = cnt * skk - sk * sk;
            let fit = if den.abs() > 1e-9 {
                let c1 = (cnt * skt - sk * st) / den;
                let c0 = (st - c1 * sk) / cnt;
                (c0 + c1 * i as f64) as f32
            } else {
                raw[i]
            };
            // Keep real expressive deviations larger than a frame or so.
            let t = if (fit - raw[i]).abs() < 1.5 { fit } else { raw[i] };
            t * hop_dt + ONSET_BIAS_S
        })
        .collect()
}

/// Picks which of every 4 beats is the bar start. Per beat: kick accent (low band), backbeat accent
/// (mid band, which belongs on 2 and 4) and harmonic change (spectral-shape difference between the
/// beat before and after). Each is z-scored over the song; the phase with the best mean wins.
/// Returns (index of first downbeat in 0..4, confidence 0..1).
fn choose_downbeat(beats: &[f32], kick: &[f32], mid: &[f32], spec: &[[f32; N_SPEC]], hop_dt: f32) -> (usize, f32) {
    let nb = beats.len();
    let frames = kick.len();
    if nb < 12 || frames == 0 {
        return (0, 0.0);
    }
    let fr = |t: f32| ((t / hop_dt).round().max(0.0) as usize).min(frames - 1);
    let peak = |env: &[f32], t: f32| -> f32 {
        let f = fr(t);
        let mut m = 0.0f32;
        for j in f.saturating_sub(2)..=(f + 2).min(frames - 1) {
            m = m.max(env[j]);
        }
        m
    };
    let mean_spec = |t0: f32, t1: f32| -> [f32; N_SPEC] {
        let (a, z) = (fr(t0), fr(t1).max(fr(t0) + 1).min(frames));
        let mut m = [0.0f32; N_SPEC];
        for f in a..z {
            for k in 0..N_SPEC {
                m[k] += spec[f][k];
            }
        }
        for v in m.iter_mut() {
            *v /= (z - a).max(1) as f32;
        }
        m
    };
    let mut kz = Vec::with_capacity(nb);
    let mut sz = Vec::with_capacity(nb);
    let mut hz = Vec::with_capacity(nb);
    for i in 0..nb {
        kz.push(peak(kick, beats[i]));
        sz.push(peak(mid, beats[i]));
        let h = if i >= 1 && i + 1 < nb {
            // Skip the first ~60 ms after the beat so the hit itself does not count as a change.
            let before = mean_spec(beats[i - 1] + 0.06, beats[i]);
            let after = mean_spec(beats[i] + 0.06, beats[i + 1]);
            // Harmonic region only (roughly 80 Hz - 2.5 kHz).
            (3..18).map(|k| (after[k] - before[k]).abs()).sum::<f32>()
        } else {
            0.0
        };
        hz.push(h);
    }
    for v in [&mut kz, &mut sz, &mut hz] {
        let (m, sd) = mean_std(v);
        for x in v.iter_mut() {
            *x = if sd > 1e-9 { (*x - m) / sd } else { 0.0 };
        }
    }
    let mut score = [0.0f32; 4];
    let mut cnt = [0.0f32; 4];
    for i in 0..nb {
        // A downbeat at phase p wants: kick here, harmonic change here, backbeat one beat later.
        let back = if i + 1 < nb { sz[i + 1] } else { 0.0 };
        let v = DOWNBEAT_W[0] * kz[i] + DOWNBEAT_W[1] * hz[i] + DOWNBEAT_W[2] * (back - sz[i]);
        score[i % 4] += v;
        cnt[i % 4] += 1.0;
    }
    for p in 0..4 {
        score[p] /= cnt[p].max(1.0);
    }
    let mut order = [0usize, 1, 2, 3];
    order.sort_by(|&a, &b| score[b].partial_cmp(&score[a]).unwrap_or(std::cmp::Ordering::Equal));
    let conf = ((score[order[0]] - score[order[1]]) / 0.4).clamp(0.0, 1.0);
    (order[0], conf)
}

/// Labels sections relative to the song's own dynamics. Each section's mean energy score is ranked
/// by duration among all sections: the quietest ~22% of the song is Calm, the loudest ~38% Intense,
/// the rest Normal. An Intense section entered from a clearly quieter one becomes a Drop. Small
/// differences near a threshold keep the previous label so labels do not flap.
fn label_sections(edges: &[f32], score: &[f32], intensity: &[f32], hop_dt: f32) -> Vec<Section> {
    let frames = score.len();
    let mut raw: Vec<(f32, f32, f32, f32)> = Vec::new(); // t0, t1, mean score, mean intensity
    for w in edges.windows(2) {
        let (t0, t1) = (w[0], w[1]);
        if t1 - t0 < 0.05 || frames == 0 {
            continue;
        }
        let a = ((t0 / hop_dt) as usize).min(frames - 1);
        let z = ((t1 / hop_dt) as usize).clamp(a + 1, frames);
        let len = (z - a) as f32;
        let sc = score[a..z].iter().sum::<f32>() / len;
        let si = intensity[a..z].iter().sum::<f32>() / len;
        raw.push((t0, t1, sc, si));
    }
    let total: f32 = raw.iter().map(|r| r.1 - r.0).sum::<f32>().max(1e-3);
    let (_, sc_sd) = mean_std(&raw.iter().map(|r| r.2).collect::<Vec<f32>>());
    let mut out: Vec<Section> = Vec::with_capacity(raw.len());
    let mut prev_q = 0.5f32;
    for (idx, r) in raw.iter().enumerate() {
        // Duration-weighted quantile of this section's score among all sections.
        let mut below = 0.0f32;
        let mut same = 0.0f32;
        for q in &raw {
            if q.2 < r.2 {
                below += q.1 - q.0;
            } else if q.2 == r.2 {
                same += q.1 - q.0;
            }
        }
        let qn = (below + 0.5 * same) / total;
        let prev = out.last().copied();
        let pick = |qv: f32| {
            if qv < SECTION_CALM_Q {
                SectionKind::Calm
            } else if qv > SECTION_INTENSE_Q {
                SectionKind::Intense
            } else {
                SectionKind::Normal
            }
        };
        let mut kind = pick(qn);
        if let Some(p) = prev {
            let pk = if p.kind == SectionKind::Drop { SectionKind::Intense } else { p.kind };
            let near_calm = (qn - SECTION_CALM_Q).abs() < SECTION_HYST;
            let near_int = (qn - SECTION_INTENSE_Q).abs() < SECTION_HYST;
            if (near_calm && matches!(pk, SectionKind::Calm | SectionKind::Normal) && matches!(kind, SectionKind::Calm | SectionKind::Normal))
                || (near_int && matches!(pk, SectionKind::Intense | SectionKind::Normal) && matches!(kind, SectionKind::Intense | SectionKind::Normal))
            {
                kind = pk;
            }
            let pr = &raw[idx - 1];
            if kind == SectionKind::Intense
                && matches!(p.kind, SectionKind::Calm | SectionKind::Normal)
                && sc_sd > 1e-6
                && ((r.2 - pr.2) / sc_sd > DROP_JUMP_SD || qn - prev_q >= DROP_JUMP_Q)
            {
                kind = SectionKind::Drop;
            }
        }
        out.push(Section { t0: r.0, t1: r.1, intensity: r.3, kind });
        prev_q = qn;
    }
    out
}

/// Chance-corrected fraction of strong onsets that land within 25 ms of the beat grid's 16th-note
/// (or triplet) subdivisions: ~0 for a grid unrelated to the music, ~0.7-0.9 for a well-locked grid.
fn grid_fit(onsets: &[Onset], beats: &[f32]) -> f32 {
    if beats.len() < 8 {
        return 0.0;
    }
    let (mut hit, mut chance, mut n) = ([0.0f32; 2], [0.0f32; 2], 0.0f32);
    for o in onsets.iter().filter(|o| o.strength >= 0.5) {
        let i = beats.partition_point(|&b| b <= o.t);
        if i == 0 || i >= beats.len() {
            continue;
        }
        let (a, b) = (beats[i - 1], beats[i]);
        if b <= a {
            continue;
        }
        for (g, div) in [4.0f32, 3.0].iter().enumerate() {
            let sub = (b - a) / div;
            let x = (o.t - a) / sub;
            let d = (x - x.round()).abs() * sub;
            if d <= 0.025 {
                hit[g] += 1.0;
            }
            chance[g] += (0.05 / sub).min(1.0);
        }
        n += 1.0;
    }
    if n < 16.0 {
        return 0.0;
    }
    // Binary (16ths) or ternary (triplet 8ths, for shuffle / 6/8), whichever fits better.
    (0..2)
        .map(|g| {
            let (h, c) = (hit[g] / n, chance[g] / n);
            ((h - c) / (1.0 - c).max(0.05)).clamp(0.0, 1.0)
        })
        .fold(0.0f32, f32::max)
}

/// Normalised autocorrelation of the onset envelope at one and two beat periods (0..1): how
/// strongly the music repeats at the detected tempo.
fn periodicity(o: &[f32], bpm: f32, hop_dt: f32) -> f32 {
    if bpm <= 1.0 {
        return 0.0;
    }
    let p = 60.0 / bpm / hop_dt;
    let n = o.len();
    let (m, _) = mean_std(o);
    let ac = |lag: usize| -> f32 {
        if lag == 0 || lag >= n {
            return 0.0;
        }
        let mut s = 0.0f64;
        for i in 0..n - lag {
            s += ((o[i] - m) * (o[i + lag] - m)) as f64;
        }
        (s / (n - lag) as f64) as f32
    };
    let a0 = ac_zero(o, m);
    if a0 <= 1e-9 {
        return 0.0;
    }
    let best = |c: f32| -> f32 {
        let l = c.round() as usize;
        ac(l.saturating_sub(1)).max(ac(l)).max(ac(l + 1))
    };
    (0.5 * (best(p) + best(2.0 * p)) / a0).clamp(0.0, 1.0)
}

fn ac_zero(o: &[f32], m: f32) -> f32 {
    o.iter().map(|v| (v - m) * (v - m)).sum::<f32>() / o.len().max(1) as f32
}

// ---------------------------------------------------------------------------------------------
// Offline analysis
// ---------------------------------------------------------------------------------------------

pub fn analyze(mono: &[f32], sr: u32, progress: &AtomicU32) -> Analysis {
    let mut sp = Spectral::new(sr);
    let (n, hop) = (sp.n, sp.hop);
    let hop_dt = sp.hop_dt();
    let duration = mono.len() as f32 / sr as f32;
    let frames = mono.len() / hop + 1;

    let mut rms = Vec::with_capacity(frames);
    let mut flux: [Vec<f32>; 3] = [Vec::with_capacity(frames), Vec::with_capacity(frames), Vec::with_capacity(frames)];
    let mut energy: [Vec<f32>; 3] = [Vec::with_capacity(frames), Vec::with_capacity(frames), Vec::with_capacity(frames)];
    let mut spec: Vec<[f32; N_SPEC]> = Vec::with_capacity(frames);
    let mut kick: Vec<f32> = Vec::with_capacity(frames);
    let mut frame = vec![0.0f32; n];
    for i in 0..frames {
        let center = (i * hop) as isize;
        let start = center - (n / 2) as isize;
        for (j, slot) in frame.iter_mut().enumerate() {
            let idx = start + j as isize;
            *slot = if idx >= 0 && (idx as usize) < mono.len() { mono[idx as usize] } else { 0.0 };
        }
        let raw = sp.process(&frame);
        rms.push(raw.rms);
        for b in 0..3 {
            flux[b].push(raw.flux[b]);
            energy[b].push(raw.energy[b]);
        }
        spec.push(raw.spec);
        kick.push(raw.kick);
        if i % 512 == 0 {
            progress.store((i * 1000 / frames) as u32, Ordering::Relaxed);
        }
    }

    // ---- onsets ----
    let params = [
        PickParams { min_gap: 0.11, k_mean: 1.5, delta: 0.14 },
        PickParams { min_gap: 0.09, k_mean: 1.6, delta: 0.16 },
        PickParams { min_gap: 0.06, k_mean: 1.6, delta: 0.18 },
    ];
    let bands = [Band::Low, Band::Mid, Band::High];
    let mut onsets: Vec<Onset> = Vec::new();
    for b in 0..3 {
        let env = if b == 0 { &kick } else { &flux[b] };
        onsets.extend(pick_onsets(env, &rms, hop_dt, bands[b], &params[b]));
    }
    onsets.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());

    // ---- combined onset envelope for tempo / beats ----
    let mut comb = vec![0.0f32; frames];
    let weights = [1.5f32, 1.0, 0.5];
    for b in 0..3 {
        let p = percentile(&flux[b], 0.98).max(1e-6);
        for i in 0..frames {
            comb[i] += weights[b] * (flux[b][i] / p).min(2.0);
        }
    }
    let local = smooth(&comb, ((0.5 / hop_dt) as usize).max(2));
    for i in 0..frames {
        comb[i] = (comb[i] - local[i]).max(0.0);
    }
    let (_, sd) = mean_std(&comb);
    if sd > 1e-9 {
        for v in comb.iter_mut() {
            *v /= sd;
        }
    }

    let period = estimate_tempo(&comb, hop_dt);
    let (bpm, mut beats): (f32, Vec<f32>) = match period {
        Some(p0) => {
            let (mut p, mut b) = choose_metrical_level(&comb, p0, hop_dt);
            // Very slow picks are almost always half-time readings of a faster pulse, and a sluggish
            // pulse plays badly; prefer the double tempo.
            if 60.0 / (p * hop_dt) < MIN_PLAY_BPM {
                p *= 0.5;
                b = track_beats(&comb, p);
            }
            (60.0 / (p * hop_dt), refine_beats(&comb, &b, hop_dt))
        }
        None => (0.0, vec![]),
    };
    let beat_len = if bpm > 1.0 { 60.0 / bpm } else { 0.5 };
    if beats.len() < 4 {
        // No usable beat structure: fall back to a plain grid so the game still has a pulse.
        beats = (0..)
            .map(|i| i as f32 * beat_len)
            .take_while(|&t| t < duration)
            .collect();
    } else {
        // Extend the grid to cover the whole track.
        let first_iv = (beats[1] - beats[0]).clamp(beat_len * 0.5, beat_len * 2.0);
        let mut t = beats[0] - first_iv;
        let mut pre = Vec::new();
        while t > 0.0 {
            pre.push(t);
            t -= first_iv;
        }
        pre.reverse();
        let last_iv = (beats[beats.len() - 1] - beats[beats.len() - 2]).clamp(beat_len * 0.5, beat_len * 2.0);
        let mut t = beats[beats.len() - 1] + last_iv;
        let mut post = Vec::new();
        while t < duration {
            post.push(t);
            t += last_iv;
        }
        pre.extend(beats);
        pre.extend(post);
        beats = pre;
    }

    // ---- downbeat phase: kick on 1 (and 3), backbeat on 2 and 4, chords change on the bar line ----
    let (downbeat, downbeat_conf) = choose_downbeat(&beats, &kick, &flux[1], &spec, hop_dt);
    let fit16 = grid_fit(&onsets, &beats);
    let per_ac = periodicity(&comb, bpm, hop_dt);
    let beat_conf = if period.is_some() && bpm > 1.0 {
        let x = 0.7 * fit16 + 0.3 * (per_ac * 1.6).min(1.0);
        let u = ((x - BEAT_CONF_LO) / (BEAT_CONF_HI - BEAT_CONF_LO)).clamp(0.0, 1.0);
        u * u * (3.0 - 2.0 * u)
    } else {
        0.0
    };

    // ---- normalised features ----
    let rms_ref = percentile(&rms, 0.98).max(1e-4);
    let e_ref = [
        percentile(&energy[0], 0.98).max(1e-6),
        percentile(&energy[1], 0.98).max(1e-6),
        percentile(&energy[2], 0.98).max(1e-6),
    ];
    let flat: Vec<f32> = spec.iter().step_by(4).flat_map(|s| s.iter().copied()).collect();
    let spec_ref = percentile(&flat, 0.98).max(1e-4);
    let feats: Vec<Feat> = (0..frames)
        .map(|i| {
            let mut s = [0.0; N_SPEC];
            for k in 0..N_SPEC {
                s[k] = (spec[i][k] / spec_ref).min(1.2);
            }
            Feat {
                rms: (rms[i] / rms_ref).min(1.3),
                low: (energy[0][i] / e_ref[0]).min(1.3),
                mid: (energy[1][i] / e_ref[1]).min(1.3),
                high: (energy[2][i] / e_ref[2]).min(1.3),
                spec: s,
            }
        })
        .collect();

    // ---- silences ----
    let rms_s = smooth(&rms, ((0.15 / hop_dt) as usize).max(1));
    let mut silences = Vec::new();
    let mut start: Option<usize> = None;
    for i in 0..=frames {
        let quiet = i < frames && rms_s[i] < 0.004;
        match (quiet, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let (a, b) = (s as f32 * hop_dt, i as f32 * hop_dt);
                if b - a >= 1.0 || s == 0 {
                    silences.push((a, b.min(duration)));
                }
                start = None;
            }
            _ => {}
        }
    }

    // ---- intensity: relative to the song itself ----
    // A per-frame "energy" score from loudness, onset density, arrangement fullness and low end,
    // z-scored over the audible part of the song, then rank-normalised so every song spans 0..1
    // with a mean near 0.5 (a quiet ballad and a brick-walled banger get the same spread).
    let db: Vec<f32> = rms.iter().map(|r| 20.0 * (r + 1e-5).log10()).collect();
    let loud = smooth(&db, ((1.5 / hop_dt) as usize).max(1));
    let audible_mask: Vec<bool> = (0..frames).map(|i| rms_s[i] >= 0.004).collect();
    let mut dens = vec![0.0f32; frames];
    for o in &onsets {
        let i = ((o.t / hop_dt).max(0.0) as usize).min(frames - 1);
        dens[i] += 0.5 + o.strength;
    }
    let dens = smooth(&dens, ((2.0 / hop_dt) as usize).max(1));
    let full_raw: Vec<f32> = feats.iter().map(|f| f.spec.iter().sum::<f32>() / N_SPEC as f32).collect();
    let full = smooth(&full_raw, ((1.5 / hop_dt) as usize).max(1));
    let low_s = smooth(&feats.iter().map(|f| f.low).collect::<Vec<f32>>(), ((1.5 / hop_dt) as usize).max(1));
    let zs = |x: &[f32]| -> Vec<f32> {
        let aud: Vec<f32> = (0..frames).filter(|&i| audible_mask[i]).map(|i| x[i]).collect();
        let (m, sd) = if aud.len() > 16 { mean_std(&aud) } else { mean_std(x) };
        x.iter().map(|v| if sd > 1e-9 { (v - m) / sd } else { 0.0 }).collect()
    };
    let (zl, zd, zf, zb) = (zs(&loud), zs(&dens), zs(&full), zs(&low_s));
    let energy_score: Vec<f32> = (0..frames)
        .map(|i| INTENSITY_W[0] * zl[i] + INTENSITY_W[1] * zd[i] + INTENSITY_W[2] * zf[i] + INTENSITY_W[3] * zb[i])
        .collect();
    let mut sorted: Vec<f32> = (0..frames).filter(|&i| audible_mask[i]).map(|i| energy_score[i]).collect();
    if sorted.len() < 16 {
        sorted = energy_score.clone();
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = |v: f32| -> f32 {
        let i = sorted.partition_point(|&x| x < v);
        let j = sorted.partition_point(|&x| x <= v);
        ((i + j) as f32 * 0.5 / sorted.len().max(1) as f32).clamp(0.0, 1.0)
    };
    let intensity: Vec<f32> = (0..frames)
        .map(|i| if audible_mask[i] { rank(energy_score[i]) } else { 0.5 * rank(energy_score[i]) })
        .collect();

    // ---- sections from bar-level novelty ----
    let mut bars: Vec<f32> = beats.iter().skip(downbeat).step_by(4).copied().collect();
    if bars.len() < 4 {
        bars = (0..).map(|i| i as f32 * 2.0).take_while(|&t| t < duration).collect();
    }
    let nb = bars.len();
    let bar_feat = |b: usize| -> [f32; 5] {
        let t0 = bars[b];
        let t1 = if b + 1 < nb { bars[b + 1] } else { duration };
        let a = ((t0 / hop_dt) as usize).min(frames - 1);
        let z = ((t1 / hop_dt) as usize).clamp(a + 1, frames);
        let len = (z - a) as f32;
        let mut f = [0.0f32; 5];
        for i in a..z {
            f[0] += intensity[i];
            f[1] += feats[i].low;
            f[2] += feats[i].mid;
            f[3] += feats[i].high;
            f[4] += comb[i];
        }
        for v in f.iter_mut() {
            *v /= len;
        }
        f
    };
    let mut bf: Vec<[f32; 5]> = (0..nb).map(bar_feat).collect();
    for d in 0..5 {
        let col: Vec<f32> = bf.iter().map(|f| f[d]).collect();
        let (m, s) = mean_std(&col);
        for f in bf.iter_mut() {
            f[d] = if s > 1e-6 { (f[d] - m) / s } else { 0.0 };
        }
    }
    let k = 3usize;
    let mut nov = vec![0.0f32; nb];
    for b in 2..nb.saturating_sub(1) {
        let a0 = b.saturating_sub(k);
        let z1 = (b + k).min(nb);
        let mut d2 = 0.0;
        for d in 0..5 {
            let before = bf[a0..b].iter().map(|f| f[d]).sum::<f32>() / (b - a0) as f32;
            let after = bf[b..z1].iter().map(|f| f[d]).sum::<f32>() / (z1 - b) as f32;
            d2 += (after - before) * (after - before);
        }
        nov[b] = d2.sqrt();
    }
    let (nm, ns) = mean_std(&nov);
    let mut bounds: Vec<usize> = vec![];
    for b in 2..nb.saturating_sub(2) {
        let lo = b.saturating_sub(3);
        let hi = (b + 3).min(nb - 1);
        if nov[b] > nm + 0.5 * ns && (lo..=hi).all(|j| nov[j] <= nov[b]) {
            if bounds.last().map(|&l| b - l >= 4).unwrap_or(true) {
                bounds.push(b);
            }
        }
    }
    // Split over-long stretches so every part of the song gets some variety.
    let mut edges: Vec<f32> = vec![0.0];
    edges.extend(bounds.iter().map(|&b| bars[b]));
    edges.push(duration);
    let mut refined: Vec<f32> = vec![0.0];
    for w in edges.windows(2) {
        let (a, z) = (w[0], w[1]);
        let len = z - a;
        if len > 30.0 {
            let parts = (len / 22.0).round().max(2.0) as usize;
            for p in 1..parts {
                let target = a + len * p as f32 / parts as f32;
                // snap to the nearest bar
                let snapped = bars
                    .iter()
                    .copied()
                    .min_by(|x, y| (x - target).abs().partial_cmp(&(y - target).abs()).unwrap())
                    .unwrap_or(target);
                if snapped > refined.last().copied().unwrap_or(0.0) + 6.0 && snapped < z - 6.0 {
                    refined.push(snapped);
                }
            }
        }
        if z > refined.last().copied().unwrap_or(0.0) + 0.01 {
            refined.push(z);
        }
    }
    let sections = label_sections(&refined, &energy_score, &intensity, hop_dt);
    let mut sections = sections;
    if sections.is_empty() {
        sections.push(Section { t0: 0.0, t1: duration, intensity: 0.5, kind: SectionKind::Normal });
    }

    // ---- sustained mid-band phrases (drive laser sweeps) ----
    let mid_s = smooth(&energy[1], ((0.12 / hop_dt) as usize).max(1));
    let mid_l = smooth(&energy[1], ((3.0 / hop_dt) as usize).max(1));
    let mid_ref = percentile(&mid_s, 0.90).max(1e-6);
    let mut phrases = Vec::new();
    let mut st: Option<usize> = None;
    for i in 0..=frames {
        let on = i < frames
            && mid_s[i] > 0.3 * mid_ref
            && match st {
                None => mid_s[i] > 1.18 * mid_l[i],
                Some(_) => mid_s[i] > 0.98 * mid_l[i],
            };
        match (on, st) {
            (true, None) => st = Some(i),
            (false, Some(s)) => {
                let (a, b) = (s as f32 * hop_dt, i as f32 * hop_dt);
                if b - a >= 0.5 {
                    phrases.push((a, b.min(a + 4.0)));
                }
                st = None;
            }
            _ => {}
        }
    }

    // ---- content seed ----
    let seed = crate::rng::fnv1a(
        mono.iter()
            .step_by((mono.len() / 50_000).max(1))
            .flat_map(|s| ((s * 32767.0) as i16).to_le_bytes()),
    ) ^ (mono.len() as u64).wrapping_mul(0x9E37_79B9);

    progress.store(1000, Ordering::Relaxed);
    Analysis { duration, hop_dt, feats, intensity, onsets, bpm, beats, downbeat, sections, phrases, silences, seed, beat_conf, downbeat_conf }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 30 s of kick-like thumps at a known tempo.
    fn click_track(bpm: f32, sr: u32) -> Vec<f32> {
        let n = (30.0 * sr as f32) as usize;
        let mut out = vec![0.0f32; n];
        let beat = 60.0 / bpm;
        let mut t = 0.5f32;
        while t < 29.0 {
            let i0 = (t * sr as f32) as usize;
            for k in 0..(0.2 * sr as f32) as usize {
                let x = k as f32 / sr as f32;
                if i0 + k < n {
                    out[i0 + k] += (std::f32::consts::TAU * (50.0 + 80.0 * (-x * 30.0).exp()) * x).sin() * (-x * 14.0).exp() * 0.8;
                }
            }
            t += beat;
        }
        out
    }

    struct Lcg(u64);
    impl Lcg {
        fn f(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f32) / (1u64 << 31) as f32
        }
    }

    /// A busy synthetic mix with percussive hits at exactly known sample positions.
    /// Returns (audio, kick times, snare times, hat times, beat times).
    pub(super) fn busy_mix(bpm: f32, sr: u32, secs: f32, seed: u64) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) {
        let srf = sr as f32;
        let n = (secs * srf) as usize;
        let mut out = vec![0.0f32; n];
        let mut rng = Lcg(seed);
        let beat = 60.0 / bpm;
        // Start at an arbitrary sub-hop offset so hits do not lock to the analysis frames.
        let t0 = 1.0 + rng.f() * 0.05;
        let (mut kicks, mut snares, mut hats, mut beats) = (vec![], vec![], vec![], vec![]);
        let mut k = 0usize;
        loop {
            let t = t0 + k as f32 * beat;
            if t > secs - 1.0 {
                break;
            }
            beats.push(t);
            let pos = k % 4;
            if pos == 0 || pos == 2 {
                kicks.push(t);
            }
            if pos == 2 && k % 8 == 6 {
                kicks.push(t + beat * 0.75);
            }
            if pos == 1 || pos == 3 {
                snares.push(t);
            }
            for h in 0..2 {
                hats.push(t + h as f32 * beat * 0.5);
            }
            k += 1;
        }
        let tau = std::f32::consts::TAU;
        let add = |out: &mut Vec<f32>, t: f32, len: f32, f: &mut dyn FnMut(f32) -> f32| {
            let i0 = (t * srf).round() as usize;
            for j in 0..(len * srf) as usize {
                if i0 + j < out.len() {
                    out[i0 + j] += f(j as f32 / srf);
                }
            }
        };
        for &t in &kicks {
            add(&mut out, t, 0.3, &mut |x| (tau * (50.0 + 90.0 * (-x * 25.0).exp()) * x).sin() * (-x * 9.0).exp() * 0.7);
        }
        let mut nz = Lcg(seed ^ 0xABCD);
        for &t in &snares {
            add(&mut out, t, 0.25, &mut |x| {
                let noise = nz.f() * 2.0 - 1.0;
                ((tau * 185.0 * x).sin() * 0.5 + noise * 0.6) * (-x * 18.0).exp() * 0.5
            });
        }
        let mut prev = 0.0f32;
        for &t in &hats {
            add(&mut out, t, 0.06, &mut |x| {
                let w = nz.f() * 2.0 - 1.0;
                let hp = w - prev;
                prev = w;
                hp * (-x * 70.0).exp() * 0.12
            });
        }
        // Bass (smoothly gliding, no attacks) and a sustained pad.
        for i in 0..n {
            let t = i as f32 / srf;
            let bar = ((t - t0) / (4.0 * beat)).floor();
            let f = 55.0 * if bar as i64 % 2 == 0 { 1.0 } else { 1.122 };
            let fade = (t.min(secs - t) / 0.5).min(1.0);
            out[i] += (tau * f * t).sin() * 0.18 * fade;
            out[i] += ((tau * 220.0 * t).sin() + (tau * 277.2 * t).sin() + (tau * 329.6 * t).sin() + (tau * 440.7 * t).sin()) * 0.03 * fade;
        }
        (out, kicks, snares, hats, beats)
    }

    /// Signed timing errors (s) of detected onsets matched to the nearest true event within 50 ms.
    pub(super) fn signed_errors(det: &[f32], truth: &[f32]) -> Vec<f32> {
        det.iter()
            .filter_map(|&d| {
                let e = truth.iter().map(|&t| d - t).min_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap())?;
                if e.abs() < 0.05 { Some(e) } else { None }
            })
            .collect()
    }

    fn err_stats(e: &[f32]) -> (f32, f32) {
        let mean = e.iter().sum::<f32>() / e.len().max(1) as f32;
        let mut a: Vec<f32> = e.iter().map(|x| x.abs()).collect();
        a.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let p95 = if a.is_empty() { 1.0 } else { a[((a.len() - 1) as f32 * 0.95) as usize] };
        (mean, p95)
    }

    #[test]
    fn onset_timing_is_unbiased() {
        for sr in [44_100u32, 48_000] {
            for (bpm, seed) in [(97.0f32, 1u64), (124.0, 2), (141.0, 3)] {
                let (mono, kicks, snares, hats, beats) = busy_mix(bpm, sr, 30.0, seed);
                let an = analyze(&mono, sr, &AtomicU32::new(0));
                let mut hh = hats.clone();
                hh.extend(snares.iter().copied());
                for (band, truth) in [(Band::Low, &kicks), (Band::Mid, &snares), (Band::High, &hh)] {
                    let det: Vec<f32> = an.onsets.iter().filter(|o| o.band == band && o.strength > 0.3).map(|o| o.t).collect();
                    let e = signed_errors(&det, truth);
                    let (mean, p95) = err_stats(&e);
                    println!("sr {sr} bpm {bpm} {:?}: matched {} / det {} / true {}  mean {:+.1} ms  p95 {:.1} ms", band, e.len(), det.len(), truth.len(), mean * 1000.0, p95 * 1000.0);
                    assert!(mean.abs() <= 0.005 && p95 <= 0.015, "sr {sr} bpm {bpm} {:?}: mean {:.1} ms p95 {:.1} ms", band, mean * 1e3, p95 * 1e3);
                    if band != Band::High {
                        assert!(e.len() as f32 >= 0.9 * truth.len() as f32, "{:?}: only {} of {} hits found", band, e.len(), truth.len());
                    }
                }
                let e = signed_errors(&an.beats, &beats);
                let (mean, p95) = err_stats(&e);
                println!("sr {sr} bpm {bpm} beats: bpm det {:.2} n {} mean {:+.1} ms p95 {:.1} ms", an.bpm, e.len(), mean * 1000.0, p95 * 1000.0);
            }
        }
    }

    #[test]
    fn downbeat_follows_bar_lines() {
        for (bpm, seed) in [(97.0f32, 4u64), (124.0, 5), (141.0, 6), (110.0, 7)] {
            let (mono, _, _, _, beats) = busy_mix(bpm, 44_100, 40.0, seed);
            let an = analyze(&mono, 44_100, &AtomicU32::new(0));
            // The true bar starts at beats[0] (bass changes there, kick on 1 and 3, snare on 2 and 4).
            let j = an.beats.iter().enumerate().min_by(|a, b| (a.1 - beats[0]).abs().partial_cmp(&(b.1 - beats[0]).abs()).unwrap()).unwrap().0;
            assert_eq!(j % 4, an.downbeat % 4, "bpm {bpm}: bar phase off");
        }
    }

    /// Kicks in a 3+3+2 eighth-note pattern (the classic 4:3 tempo trap) at a known tempo.
    fn three_three_two(bpm: f32, sr: u32) -> Vec<f32> {
        let srf = sr as f32;
        let n = (40.0 * srf) as usize;
        let mut out = vec![0.0f32; n];
        let beat = 60.0 / bpm;
        let tau = std::f32::consts::TAU;
        let mut nz = Lcg(99);
        let mut bar = 0;
        loop {
            let t0 = 0.5 + bar as f32 * 4.0 * beat;
            if t0 > 38.0 {
                break;
            }
            for &e in &[0.0f32, 3.0, 6.0] {
                let i0 = ((t0 + e * beat * 0.5) * srf) as usize;
                for k in 0..(0.25 * srf) as usize {
                    let x = k as f32 / srf;
                    if i0 + k < n {
                        out[i0 + k] += (tau * (50.0 + 80.0 * (-x * 30.0).exp()) * x).sin() * (-x * 12.0).exp() * 0.8;
                    }
                }
            }
            for b in [1.0f32, 3.0] {
                let i0 = ((t0 + b * beat) * srf) as usize;
                for k in 0..(0.15 * srf) as usize {
                    let x = k as f32 / srf;
                    if i0 + k < n {
                        out[i0 + k] += (nz.f() * 2.0 - 1.0) * (-x * 25.0).exp() * 0.4;
                    }
                }
            }
            for h in 0..8 {
                let i0 = ((t0 + h as f32 * beat * 0.5) * srf) as usize;
                for k in 0..(0.03 * srf) as usize {
                    let x = k as f32 / srf;
                    if i0 + k < n {
                        out[i0 + k] += (nz.f() * 2.0 - 1.0) * (-x * 90.0).exp() * 0.08;
                    }
                }
            }
            bar += 1;
        }
        out
    }

    #[test]
    fn tresillo_kicks_do_not_fool_tempo() {
        for bpm in [96.0f32, 100.0, 105.0] {
            let mono = three_three_two(bpm, 44_100);
            let an = analyze(&mono, 44_100, &AtomicU32::new(0));
            let r = an.bpm / bpm;
            assert!((r - 1.0).abs() < 0.03 || (r - 2.0).abs() < 0.06, "{bpm} BPM 3+3+2 detected as {}", an.bpm);
        }
    }

    #[test]
    fn beat_times_are_unbiased() {
        for sr in [44_100u32, 48_000] {
            for (bpm, seed) in [(97.0f32, 11u64), (124.0, 12), (141.0, 13)] {
                let (mono, _, _, _, beats) = busy_mix(bpm, sr, 30.0, seed);
                let an = analyze(&mono, sr, &AtomicU32::new(0));
                let e = signed_errors(&an.beats, &beats);
                let (mean, p95) = err_stats(&e);
                assert!(e.len() as f32 >= 0.9 * beats.len() as f32, "only {} of {} beats matched", e.len(), beats.len());
                assert!(mean.abs() <= 0.005 && p95 <= 0.015, "sr {sr} bpm {bpm}: beat mean {:.1} ms p95 {:.1} ms", mean * 1e3, p95 * 1e3);
                assert!(an.beat_conf > 0.7, "beat_conf {} on a steady groove", an.beat_conf);
            }
        }
    }

    /// Quiet intro, verse, loud chorus, breakdown, chorus, through a brick-wall limiter.
    fn compressed_song(sr: u32) -> Vec<f32> {
        let (mix, ..) = busy_mix(120.0, sr, 96.0, 21);
        let srf = sr as f32;
        let tau = std::f32::consts::TAU;
        let mut nz = Lcg(77);
        let mut out = vec![0.0f32; mix.len()];
        for i in 0..mix.len() {
            let t = i as f32 / srf;
            let pad = ((tau * 220.0 * t).sin() + (tau * 330.0 * t).sin()) * 0.15;
            // 0-16 intro (pad), 16-40 verse (groove), 40-64 chorus (groove + noise wash), 64-72 breakdown, 72-96 chorus
            let (g, wash) = match t {
                t if t < 16.0 => (0.0, 0.0),
                t if t < 40.0 => (1.0, 0.0),
                t if t < 64.0 => (1.0, 1.0),
                t if t < 72.0 => (0.0, 0.0),
                _ => (1.0, 1.0),
            };
            let x = pad + g * mix[i] + wash * ((nz.f() * 2.0 - 1.0) * 0.25 + mix[i] * 0.6);
            out[i] = (x * 4.0).tanh() * 0.9;
        }
        out
    }

    #[test]
    fn sections_follow_song_structure_on_compressed_master() {
        let sr = 44_100;
        let an = analyze(&compressed_song(sr), sr, &AtomicU32::new(0));
        let kind_at = |t: f32| an.section_at(t).map(|s| s.kind).unwrap();
        assert_eq!(kind_at(8.0), SectionKind::Calm, "{:?}", an.sections);
        assert!(matches!(kind_at(52.0), SectionKind::Intense | SectionKind::Drop), "{:?}", an.sections);
        assert!(matches!(kind_at(84.0), SectionKind::Intense | SectionKind::Drop), "{:?}", an.sections);
        let hot: f32 = an.sections.iter().filter(|s| matches!(s.kind, SectionKind::Intense | SectionKind::Drop)).map(|s| s.t1 - s.t0).sum();
        assert!(hot / an.duration < 0.6, "too much of the song is Intense: {:?}", an.sections);
        let mean = an.intensity.iter().sum::<f32>() / an.intensity.len() as f32;
        assert!((0.4..0.6).contains(&mean), "intensity mean {mean}");
    }

    #[test]
    fn intensity_is_relative_to_the_song() {
        // A sparse slow song and a dense fast one should both span the scale around 0.5.
        let sr = 44_100;
        let slow: Vec<f32> = busy_mix(72.0, sr, 40.0, 31).0.iter().map(|x| x * 0.15).collect();
        let fast = compressed_song(sr);
        for (name, mono) in [("slow", slow), ("fast", fast)] {
            let an = analyze(&mono, sr, &AtomicU32::new(0));
            let mean = an.intensity.iter().sum::<f32>() / an.intensity.len() as f32;
            let hi = an.intensity.iter().filter(|&&v| v > 0.8).count() as f32 / an.intensity.len() as f32;
            assert!((0.4..0.6).contains(&mean), "{name}: intensity mean {mean}");
            assert!(hi > 0.1, "{name}: intensity never gets high");
        }
    }

    #[test]
    fn free_time_audio_has_low_beat_conf() {
        // Slowly swelling, unaccented pad chords with no pulse.
        let sr = 44_100;
        let n = 40 * sr as usize;
        let tau = std::f32::consts::TAU;
        let mono: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let env = 0.5 + 0.5 * (tau * t / 13.7).sin();
                ((tau * 196.0 * t).sin() + (tau * 246.9 * t + (t * 0.31).sin()).sin() + (tau * 293.7 * t).sin()) * 0.1 * env
            })
            .collect();
        let an = analyze(&mono, sr, &AtomicU32::new(0));
        assert!(an.beat_conf < 0.3, "beat_conf {} on a pulseless pad", an.beat_conf);
    }

    #[test]
    fn live_beat_locks_to_the_groove() {
        for (bpm, seed) in [(124.0f32, 41u64), (97.0, 42)] {
            let sr = 48_000;
            let (mono, _, _, _, beats) = busy_mix(bpm, sr, 30.0, seed);
            let mut sa = StreamAnalyzer::new(sr);
            let mut out = StreamOut::default();
            for chunk in mono.chunks(480) {
                sa.push(chunk, &mut out);
            }
            let lb = sa.live_beat();
            assert!(lb.conf > 0.3, "live conf {}", lb.conf);
            let true_p = 60.0 / bpm;
            let r = lb.period / true_p;
            assert!((r - 1.0).abs() < 0.02 || (r - 0.5).abs() < 0.01 || (r - 2.0).abs() < 0.04, "live period {} vs {}", lb.period, true_p);
            // Predicted beats near the end must sit on true beats (or true 8ths when double-time).
            let grid = if r < 0.75 { true_p * 0.5 } else { true_p };
            for k in -6..0 {
                let t = lb.phase_t + k as f32 * lb.period;
                let off = ((t - beats[0]) / grid).round() * grid + beats[0];
                assert!((t - off).abs() < 0.02, "live beat {:.3} is {:.0} ms off", t, (t - off) * 1e3);
            }
        }
    }

    #[test]
    fn finds_tempo_and_kicks() {
        for bpm in [96.0f32, 120.0, 140.0] {
            let sr = 44_100;
            let mono = click_track(bpm, sr);
            let an = analyze(&mono, sr, &AtomicU32::new(0));
            assert!((an.bpm - bpm).abs() < 1.5, "bpm {} detected as {}", bpm, an.bpm);
            let lows = an.onsets.iter().filter(|o| o.band == Band::Low).count() as f32;
            let expected = 28.5 / (60.0 / bpm);
            assert!((lows - expected).abs() <= 3.0, "{} kicks expected, {} found", expected, lows);
        }
    }

    #[test]
    fn kick_times_are_accurate() {
        let sr = 48_000;
        let mono = click_track(120.0, sr);
        let an = analyze(&mono, sr, &AtomicU32::new(0));
        // Every detected kick should sit within 25 ms of a true one (0.5 + k * 0.5 s).
        for o in an.onsets.iter().filter(|o| o.band == Band::Low) {
            let k = ((o.t - 0.5) / 0.5).round();
            let err = (o.t - (0.5 + k * 0.5)).abs();
            assert!(err < 0.025, "kick at {:.3} is {:.0} ms off", o.t, err * 1000.0);
        }
    }

    #[test]
    fn streaming_matches_offline_roughly() {
        let sr = 48_000;
        let mono = click_track(120.0, sr);
        let mut sa = StreamAnalyzer::new(sr);
        let mut out = StreamOut::default();
        for chunk in mono.chunks(480) {
            sa.push(chunk, &mut out);
        }
        let lows = out.onsets.iter().filter(|o| o.band == Band::Low).count();
        assert!((48..=60).contains(&lows), "streaming found {} kicks", lows);
    }

    #[test]
    fn silence_is_detected() {
        let sr = 44_100;
        let mut mono = click_track(120.0, sr);
        for s in mono[(10 * sr as usize)..(15 * sr as usize)].iter_mut() {
            *s = 0.0;
        }
        let an = analyze(&mono, sr, &AtomicU32::new(0));
        assert!(an.silences.iter().any(|&(a, b)| a < 11.0 && b > 14.0), "silences: {:?}", an.silences);
    }
}

// ---------------------------------------------------------------------------------------------
// Streaming analysis (live mode)
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct StreamOut {
    /// (capture-timeline seconds, features, raw RMS level)
    pub feats: Vec<(f32, Feat, f32)>,
    pub onsets: Vec<Onset>,
}

pub struct StreamAnalyzer {
    sp: Spectral,
    buf: Vec<f32>,
    frame: u64,
    env: [VecDeque<f32>; 3],
    refs: [f32; 3],
    seen: [u32; 3],
    last_onset: [i64; 3],
    norm: [f32; 4],
    spec_ref: f32,
    rms_hist: VecDeque<f32>,
    live: LiveTempo,
}

const LOOKAHEAD: usize = 3;
const HIST: usize = 48;

/// Online beat estimate for live mode: beats are predicted at `phase_t + k * period` (seconds on the
/// capture timeline). `conf` is 0 when there is no usable pulse yet.
#[derive(Clone, Copy, Debug, Default)]
pub struct LiveBeat {
    pub period: f32,
    pub phase_t: f32,
    pub conf: f32,
}

/// Onset-envelope history for the live tempo tracker (~11 s at 48 kHz). Allocated once.
const LIVE_HIST: usize = 1024;
/// Re-estimate the live tempo every this many analysis frames (~0.2 s).
const LIVE_EVERY: u64 = 20;

struct LiveTempo {
    env: Vec<f32>,
    pos: usize,
    filled: usize,
    refs: [f32; 3],
    ac: Vec<f32>,
    lin: Vec<f32>,
    pre: Vec<f32>,
    beat: LiveBeat,
    /// Candidate period (frames) waiting to replace the current one, and how often it has won.
    pending: (f32, u32),
    period_f: f32,
}

impl LiveTempo {
    fn new() -> Self {
        LiveTempo {
            env: vec![0.0; LIVE_HIST],
            pos: 0,
            filled: 0,
            refs: [1e-3; 3],
            ac: vec![0.0; LIVE_HIST / 2],
            lin: vec![0.0; LIVE_HIST],
            pre: vec![0.0; LIVE_HIST + 1],
            beat: LiveBeat::default(),
            pending: (0.0, 0),
            period_f: 0.0,
        }
    }

    fn push(&mut self, flux: &[f32; 3]) {
        let w = [1.5f32, 1.0, 0.5];
        let mut e = 0.0;
        for b in 0..3 {
            self.refs[b] = (self.refs[b] * 0.9995).max(flux[b]).max(1e-3);
            e += w[b] * (flux[b] / (0.6 * self.refs[b])).min(2.0);
        }
        self.env[self.pos] = e;
        self.pos = (self.pos + 1) % LIVE_HIST;
        self.filled = (self.filled + 1).min(LIVE_HIST);
    }

    /// `last_t`: capture time of the newest envelope frame.
    fn update(&mut self, hop_dt: f32, last_t: f32) {
        let n = self.filled;
        let lo = ((60.0 / MAX_BPM) / hop_dt).floor() as usize;
        let hi = ((60.0 / MIN_BPM) / hop_dt).ceil() as usize;
        if lo < 2 || 2 * hi + 2 >= self.ac.len() || n < (4 * hi).min(LIVE_HIST) {
            self.beat.conf = 0.0;
            return;
        }
        // Linearise (oldest first), z-score, keep the positive part.
        let start = (self.pos + LIVE_HIST - n) % LIVE_HIST;
        for i in 0..n {
            self.lin[i] = self.env[(start + i) % LIVE_HIST];
        }
        // Remove the local (+-0.5 s) mean like the offline envelope, then z-score.
        let half = ((0.5 / hop_dt) as usize).max(2);
        self.pre[0] = 0.0;
        for i in 0..n {
            self.pre[i + 1] = self.pre[i] + self.lin[i];
        }
        for i in 0..n {
            let a = i.saturating_sub(half);
            let z = (i + half + 1).min(n);
            let local = (self.pre[z] - self.pre[a]) / (z - a) as f32;
            self.lin[i] = (self.lin[i] - local).max(0.0);
        }
        let o = &mut self.lin[..n];
        let (m, sd) = mean_std(o);
        if sd < 1e-6 {
            self.beat.conf = 0.0;
            return;
        }
        for v in o.iter_mut() {
            *v = ((*v - m) / sd).max(0.0);
        }
        let o = &self.lin[..n];
        let max_lag = 2 * hi + 2;
        for lag in 1..=max_lag {
            let mut acc = 0.0f32;
            for i in 0..n - lag {
                acc += o[i] * o[i + lag];
            }
            self.ac[lag] = acc / (n - lag) as f32;
        }
        let a0 = o.iter().map(|v| v * v).sum::<f32>() / n as f32;
        let ac = &self.ac;
        let score = |lag: usize| -> f32 {
            let bpm = 60.0 / (lag as f32 * hop_dt);
            (ac[lag] + 0.5 * ac[2 * lag] + 0.15 * (ac[2 * lag - 1] + ac[2 * lag + 1])) * tempo_prior(bpm, 0.85)
        };
        let mut best = lo;
        let mut best_s = f32::MIN;
        for lag in lo..=hi {
            let sc = score(lag);
            if sc > best_s {
                best_s = sc;
                best = lag;
            }
        }
        let mut p = best as f32;
        if best > lo && best < hi {
            let (a, b, c) = (score(best - 1), score(best), score(best + 1));
            let d = a - 2.0 * b + c;
            if d.abs() > 1e-9 {
                p += (0.5 * (a - c) / d).clamp(-0.5, 0.5);
            }
        }
        // Same metrical-level check as offline: a grid with ternary subdivisions and weak beats is
        // usually 4:3 or 3:2 off.
        let comb_p = |ph: f32, pf: f32| -> f32 {
            let mut v = 0.0;
            let mut k = 0.0f32;
            loop {
                let idx = n as isize - 1 - (ph + k * pf).round() as isize;
                if idx < 0 {
                    break;
                }
                if (idx as usize) < n {
                    v += o[idx as usize] * (-k / 12.0).exp();
                }
                k += 1.0;
            }
            v
        };
        let level_score = |pf: f32| -> f32 {
            let steps = (pf.round() as usize).max(2);
            let mut bp = 0usize;
            let mut bv = f32::MIN;
            for ph in 0..steps {
                let v = comb_p(ph as f32, pf);
                if v > bv {
                    bv = v;
                    bp = ph;
                }
            }
            let ph = bp as f32;
            let sal = [
                bv,
                comb_p(ph + 0.5 * pf, pf),
                0.5 * (comb_p(ph + 0.25 * pf, pf) + comb_p(ph + 0.75 * pf, pf)),
                0.5 * (comb_p(ph + pf / 3.0, pf) + comb_p(ph + 2.0 * pf / 3.0, pf)),
            ];
            metrical_score(&sal) * tempo_prior(60.0 / (pf * hop_dt), 1.2)
        };
        let base_s = level_score(p) * TEMPO_SWITCH_MARGIN;
        let mut best_level = (p, base_s);
        for r in [4.0f32 / 3.0, 0.75, 2.0 / 3.0, 1.5] {
            let c = p * r;
            let bpm = 60.0 / (c * hop_dt);
            if !(MIN_BPM..=MAX_BPM).contains(&bpm) || 2.0 * c > (n as f32) / 2.0 {
                continue;
            }
            let sc = level_score(c);
            if sc > best_level.1 {
                best_level = (c, sc);
            }
        }
        let mut p = best_level.0;
        if 60.0 / (p * hop_dt) < MIN_PLAY_BPM {
            p *= 0.5;
        }
        // Hysteresis: keep the current period unless a different one wins several updates in a row.
        if self.period_f <= 0.0 {
            self.period_f = p;
        } else if (p / self.period_f - 1.0).abs() < 0.04 {
            self.period_f += 0.25 * (p - self.period_f);
            self.pending = (0.0, 0);
        } else if (p / self.pending.0.max(1e-3) - 1.0).abs() < 0.04 {
            self.pending.1 += 1;
            if self.pending.1 >= 5 {
                self.period_f = p;
                self.pending = (0.0, 0);
                self.beat.conf = 0.0;
            }
        } else {
            self.pending = (p, 1);
        }
        let pf = self.period_f;
        // Phase: offset (frames back from the newest frame) whose comb of past beats is strongest,
        // with recent beats weighted more.
        let comb = |ph: f32| comb_p(ph, pf);
        let steps = (pf.round() as usize).max(2);
        let mut best_ph = 0usize;
        let mut best_v = f32::MIN;
        for ph in 0..steps {
            let v = comb(ph as f32);
            if v > best_v {
                best_v = v;
                best_ph = ph;
            }
        }
        let (a, b, c) = (comb(best_ph as f32 - 1.0), best_v, comb(best_ph as f32 + 1.0));
        let d = a - 2.0 * b + c;
        let mut off = best_ph as f32;
        if best_ph > 0 && d.abs() > 1e-9 {
            off -= (0.5 * (a - c) / d).clamp(-0.5, 0.5);
        }
        let period = pf * hop_dt;
        let new_phase = last_t - off * hop_dt + ONSET_BIAS_S;
        // Smooth the phase against the previous prediction so beats do not jump.
        let phase_t = if self.beat.conf > 0.0 && self.beat.period > 0.0 {
            let k = ((new_phase - self.beat.phase_t) / period).round();
            let pred = self.beat.phase_t + k * period;
            let mut err = new_phase - pred;
            if err.abs() > 0.25 * period {
                err *= 0.3;
            }
            pred + 0.35 * err
        } else {
            new_phase
        };
        let per = if a0 > 1e-9 { (ac[best].max(ac[2 * best]) / a0).clamp(0.0, 1.0) } else { 0.0 };
        let conf = ((per - LIVE_CONF_LO) / (LIVE_CONF_HI - LIVE_CONF_LO)).clamp(0.0, 1.0).max(0.01);
        self.beat = LiveBeat { period, phase_t, conf };
    }
}

impl StreamAnalyzer {
    pub fn new(sr: u32) -> Self {
        StreamAnalyzer {
            sp: Spectral::new(sr),
            buf: Vec::new(),
            frame: 0,
            env: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
            refs: [0.0; 3],
            seen: [0; 3],
            last_onset: [-1000; 3],
            norm: [0.02, 0.02, 0.01, 0.05],
            spec_ref: 2.0,
            rms_hist: VecDeque::new(),
            live: LiveTempo::new(),
        }
    }

    pub fn hop_dt(&self) -> f32 {
        self.sp.hop_dt()
    }

    /// Current online beat estimate (cheap: returns the state updated inside `push`).
    pub fn live_beat(&self) -> LiveBeat {
        self.live.beat
    }

    /// Feed mono samples (continuous capture timeline starting at t = 0).
    pub fn push(&mut self, mono: &[f32], out: &mut StreamOut) {
        self.buf.extend_from_slice(mono);
        let (n, hop) = (self.sp.n, self.sp.hop);
        let hop_dt = self.sp.hop_dt();
        let mut offset = 0usize;
        while self.buf.len() - offset >= n {
            let raw = self.sp.process(&self.buf[offset..offset + n]);
            offset += hop;
            let k = self.frame;
            self.frame += 1;
            let t = (k as f32 * hop as f32 + n as f32 * 0.5) / self.sp.sr;

            // Running normalisation with slow decay.
            let decay = 0.9997f32;
            let vals = [raw.energy[0], raw.energy[1], raw.energy[2], raw.rms];
            let floors = [0.004f32, 0.004, 0.002, 0.02];
            for i in 0..4 {
                self.norm[i] = (self.norm[i] * decay).max(vals[i]).max(floors[i]);
            }
            let smax = raw.spec.iter().cloned().fold(0.0f32, f32::max);
            self.spec_ref = (self.spec_ref * decay).max(smax).max(0.6);
            let mut spec = [0.0; N_SPEC];
            for i in 0..N_SPEC {
                spec[i] = (raw.spec[i] / self.spec_ref).min(1.2);
            }
            out.feats.push((
                t,
                Feat {
                    rms: (raw.rms / self.norm[3]).min(1.3),
                    low: (raw.energy[0] / self.norm[0]).min(1.3),
                    mid: (raw.energy[1] / self.norm[1]).min(1.3),
                    high: (raw.energy[2] / self.norm[2]).min(1.3),
                    spec,
                },
                raw.rms,
            ));

            self.live.push(&raw.flux);
            if self.frame % LIVE_EVERY == 0 {
                self.live.update(hop_dt, t);
            }
            self.rms_hist.push_back(raw.rms);
            if self.rms_hist.len() > HIST {
                self.rms_hist.pop_front();
            }
            let gaps = [0.11f32, 0.09, 0.06];
            let bands = [Band::Low, Band::Mid, Band::High];
            for b in 0..3 {
                let env = &mut self.env[b];
                env.push_back(if b == 0 { raw.kick } else { raw.flux[b] });
                if env.len() > HIST {
                    env.pop_front();
                }
                if env.len() < 2 * LOOKAHEAD + 8 {
                    continue;
                }
                let c = env.len() - 1 - LOOKAHEAD;
                let v = env[c];
                let mut is_max = v > 0.02;
                for j in c - LOOKAHEAD..=c + LOOKAHEAD {
                    if env[j] > v {
                        is_max = false;
                        break;
                    }
                }
                if !is_max {
                    continue;
                }
                let past = c.min(34);
                let mean = env.iter().skip(c - past).take(past).sum::<f32>() / past.max(1) as f32;
                if v < 1.5 * mean {
                    continue;
                }
                self.seen[b] += 1;
                // Track the typical peak height.
                if self.refs[b] <= 0.0 {
                    self.refs[b] = v;
                } else if v > self.refs[b] {
                    self.refs[b] += 0.2 * (v - self.refs[b]);
                } else {
                    self.refs[b] += 0.04 * (v - self.refs[b]);
                }
                let cframe = k as i64 - LOOKAHEAD as i64;
                let gap = (gaps[b] / hop_dt).round() as i64;
                let rms_c = self.rms_hist[self.rms_hist.len() - 1 - LOOKAHEAD.min(self.rms_hist.len() - 1)];
                // The first few peaks only calibrate the reference level.
                if self.seen[b] < 4 || v < 0.22 * self.refs[b] || cframe - self.last_onset[b] < gap || rms_c < 0.0035 {
                    continue;
                }
                self.last_onset[b] = cframe;
                out.onsets.push(Onset {
                    t: t - LOOKAHEAD as f32 * hop_dt,
                    band: bands[b],
                    strength: (v / (1.3 * self.refs[b])).min(1.0),
                });
            }
        }
        if offset > 0 {
            self.buf.drain(0..offset);
        }
    }
}
