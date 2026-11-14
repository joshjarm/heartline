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
        if self.has_prev {
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
            }
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
        Raw { rms, flux, energy, spec }
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
        .map(|(i, v)| Onset { t: i as f32 * hop_dt, band, strength: (v / 1.3).min(1.0) })
        .collect()
}

/// Returns the beat period in frames.
fn estimate_tempo(o: &[f32], hop_dt: f32) -> Option<f32> {
    let n = o.len();
    let lo = ((60.0 / 220.0) / hop_dt).floor() as usize;
    let hi = ((60.0 / 55.0) / hop_dt).ceil() as usize;
    if n < hi * 6 || lo < 2 {
        return None;
    }
    let max_lag = hi * 2 + 2;
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
        onsets.extend(pick_onsets(&flux[b], &rms, hop_dt, bands[b], &params[b]));
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
        Some(p) => {
            let b = track_beats(&comb, p);
            (60.0 / (p * hop_dt), b.into_iter().map(|i| i as f32 * hop_dt).collect())
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

    // ---- downbeat phase: kicks tend to sit on beat 1 (and 3) ----
    let low_p = percentile(&flux[0], 0.98).max(1e-6);
    let mut phase_score = [0.0f32; 4];
    for (i, &bt) in beats.iter().enumerate() {
        let f = (bt / hop_dt).round() as isize;
        let mut best = 0.0f32;
        for d in -2..=2 {
            let j = f + d;
            if j >= 0 && (j as usize) < frames {
                best = best.max(flux[0][j as usize] / low_p);
            }
        }
        phase_score[i % 4] += best;
    }
    let downbeat = (0..4)
        .max_by(|&a, &b| phase_score[a].partial_cmp(&phase_score[b]).unwrap())
        .unwrap_or(0);

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

    // ---- intensity (loudness + onset density) ----
    let db: Vec<f32> = rms.iter().map(|r| 20.0 * (r + 1e-5).log10()).collect();
    let loud = smooth(&db, ((1.5 / hop_dt) as usize).max(1));
    // Percentiles over the audible part only, so silence does not flatten the scale.
    let audible: Vec<f32> = (0..frames).filter(|&i| rms_s[i] >= 0.004).map(|i| loud[i]).collect();
    let (p10, p95) = if audible.len() > 16 {
        (percentile(&audible, 0.10), percentile(&audible, 0.95))
    } else {
        (percentile(&loud, 0.10), percentile(&loud, 0.95))
    };
    let span = (p95 - p10).max(3.0);
    let mut dens = vec![0.0f32; frames];
    for o in &onsets {
        let i = ((o.t / hop_dt) as usize).min(frames - 1);
        dens[i] += 0.5 + o.strength;
    }
    let dens = smooth(&dens, ((2.0 / hop_dt) as usize).max(1));
    let dref = percentile(&dens, 0.95).max(1e-6);
    let intensity: Vec<f32> = (0..frames)
        .map(|i| {
            let l = ((loud[i] - p10) / span).clamp(0.0, 1.0);
            let d = (dens[i] / dref).clamp(0.0, 1.0);
            (0.65 * l + 0.35 * d).clamp(0.0, 1.0)
        })
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
    let mut sections: Vec<Section> = Vec::new();
    for w in refined.windows(2) {
        let (t0, t1) = (w[0], w[1]);
        if t1 - t0 < 0.05 {
            continue;
        }
        let a = ((t0 / hop_dt) as usize).min(frames - 1);
        let z = ((t1 / hop_dt) as usize).clamp(a + 1, frames);
        let si = intensity[a..z].iter().sum::<f32>() / (z - a) as f32;
        let prev = sections.last().copied();
        let mut kind = if si < 0.36 {
            SectionKind::Calm
        } else if si > 0.64 {
            SectionKind::Intense
        } else {
            SectionKind::Normal
        };
        if kind == SectionKind::Intense {
            if let Some(p) = prev {
                if si - p.intensity > 0.18 && p.kind != SectionKind::Intense && p.kind != SectionKind::Drop {
                    kind = SectionKind::Drop;
                }
            }
        }
        sections.push(Section { t0, t1, intensity: si, kind });
    }
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
    Analysis { duration, hop_dt, feats, intensity, onsets, bpm, beats, downbeat, sections, phrases, silences, seed }
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
}

const LOOKAHEAD: usize = 3;
const HIST: usize = 48;

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
        }
    }

    pub fn hop_dt(&self) -> f32 {
        self.sp.hop_dt()
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

            self.rms_hist.push_back(raw.rms);
            if self.rms_hist.len() > HIST {
                self.rms_hist.pop_front();
            }
            let gaps = [0.11f32, 0.09, 0.06];
            let bands = [Band::Low, Band::Mid, Band::High];
            for b in 0..3 {
                let env = &mut self.env[b];
                env.push_back(raw.flux[b]);
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
