//! Audio engine. A dedicated thread owns all cpal streams:
//!  * file playback with a sample-accurate "what is audible now" clock
//!  * live mode: WASAPI loopback capture of an output device into a ring buffer, optionally
//!    re-played to another device after a fixed delay (so the game can look ahead).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};

use crate::decode::Track;

#[derive(Clone)]
pub struct DevInfo {
    pub name: String,
    pub id: cpal::DeviceId,
    pub is_default: bool,
}

struct FileState {
    track: Option<Arc<Track>>,
    pos: f64,
    playing: bool,
    started: bool,
    finished: bool,
    stamp: Instant,
    stamp_pos: f64,
    latency: f64,
    /// Wrap around at the end instead of finishing (menu music).
    looping: bool,
    /// Current / target gain of the main voice and how fast it moves (per output frame).
    gain: f32,
    gain_target: f32,
    gain_step: f32,
    /// Stop (drop the track) once a fade-out reaches silence.
    stop_at_zero: bool,
    /// The previous track, fading out underneath the new one (crossfade).
    prev: Option<(Arc<Track>, f64, f32, f32)>,
}

/// One playing sound effect.
struct SfxVoice {
    buf: Arc<Vec<f32>>,
    pos: f64,
    gain: f32,
    pan: f32,
}

/// Sample rate the sound-effect bank is synthesised at.
const SFX_SR: f64 = 48_000.0;
const MAX_SFX_VOICES: usize = 14;

/// Sound effects. All are short, soft and unpitched or neutrally pitched so they sit under any
/// song; the ones tied to the beat (tile slams, the Heart) land on musical moments anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sfx {
    Hurt = 0,
    Shard,
    Upgrade,
    Kill,
    HeartBreak,
    Slam,
    Dash,
    Shield,
    Death,
    UiMove,
    UiSelect,
    Heal,
    NoteShot,
    Clear,
}

const SFX_COUNT: usize = 14;

fn build_sfx_bank() -> Vec<Arc<Vec<f32>>> {
    let sr = SFX_SR as f32;
    let tau = std::f32::consts::TAU;
    let mut seed = 0x2468_ACE1u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    // Render `secs` of sound from a per-sample closure, with a short fade at both ends.
    let mut make = |secs: f32, f: &mut dyn FnMut(f32, f32) -> f32| -> Arc<Vec<f32>> {
        let n = (secs * sr) as usize;
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr;
            let edge = (t * 400.0).min(1.0) * ((secs - t) * 200.0).clamp(0.0, 1.0);
            v.push(f(t, noise()) * edge);
        }
        Arc::new(v)
    };
    let mut bank: Vec<Arc<Vec<f32>>> = Vec::with_capacity(SFX_COUNT);
    // Hurt: low thud plus a short dull noise burst.
    let mut lp = 0.0f32;
    bank.push(make(0.28, &mut |t, n| {
        lp += (n - lp) * 0.12;
        (tau * (58.0 + 70.0 * (-t * 22.0).exp()) * t).sin() * (-t * 13.0).exp() * 0.8 + lp * (-t * 26.0).exp() * 0.9
    }));
    // Shard: tiny soft tick with a hint of tone.
    bank.push(make(0.05, &mut |t, _| (tau * 1560.0 * t).sin() * (-t * 90.0).exp() * 0.35));
    // Upgrade: two soft rising notes (a fifth apart).
    bank.push(make(0.5, &mut |t, _| {
        let a = (tau * 660.0 * t).sin() * (-t * 9.0).exp();
        let t2 = (t - 0.11).max(0.0);
        let b = if t > 0.11 { ((tau * 990.0 * t2).sin() + 0.3 * (tau * 1980.0 * t2).sin()) * (-t2 * 7.0).exp() } else { 0.0 };
        (a + b) * 0.4
    }));
    // Kill: short filtered pop falling in pitch.
    let mut lp = 0.0f32;
    bank.push(make(0.12, &mut |t, n| {
        lp += (n - lp) * 0.3;
        (tau * (420.0 * (-t * 30.0).exp() + 120.0) * t).sin() * (-t * 34.0).exp() * 0.5 + lp * (-t * 60.0).exp() * 0.4
    }));
    // Heart break: sub boom, noise wash and a glassy shimmer.
    let mut lp = 0.0f32;
    bank.push(make(1.6, &mut |t, n| {
        lp += (n - lp) * 0.06;
        let boom = (tau * (40.0 + 60.0 * (-t * 8.0).exp()) * t).sin() * (-t * 3.2).exp();
        let shimmer = ((tau * 1320.0 * t).sin() + (tau * 1977.0 * t).sin() + (tau * 2640.0 * t).sin()) * (-t * 2.6).exp() * 0.12;
        boom * 0.9 + lp * (-t * 5.0).exp() * 1.2 + shimmer
    }));
    // Slam: tight low thump (tile strikes; always lands on a beat).
    bank.push(make(0.2, &mut |t, _| (tau * (48.0 + 90.0 * (-t * 40.0).exp()) * t).sin() * (-t * 20.0).exp() * 0.9));
    // Dash: airy sweep.
    let (mut lp, mut lp2) = (0.0f32, 0.0f32);
    bank.push(make(0.18, &mut |t, n| {
        let k = 0.08 + 0.5 * (t / 0.18);
        lp += (n - lp) * k;
        lp2 += (lp - lp2) * 0.5;
        (lp - lp2 * 0.6) * (t * 40.0).min(1.0) * (-t * 14.0).exp() * 0.9
    }));
    // Shield: glassy ping.
    bank.push(make(0.4, &mut |t, _| ((tau * 1244.0 * t).sin() + 0.5 * (tau * 1866.0 * t).sin()) * (-t * 9.0).exp() * 0.35));
    // Death: falling tone into a thud.
    let mut lp = 0.0f32;
    bank.push(make(1.1, &mut |t, n| {
        lp += (n - lp) * 0.05;
        let f = 300.0 * (-t * 2.4).exp() + 38.0;
        (tau * f * t).sin() * (-t * 2.4).exp() * 0.7 + lp * (-t * 4.0).exp() * 0.8
    }));
    // UI move / select.
    bank.push(make(0.035, &mut |t, _| (tau * 880.0 * t).sin() * (-t * 110.0).exp() * 0.3));
    bank.push(make(0.16, &mut |t, _| ((tau * 660.0 * t).sin() + 0.5 * (tau * 1320.0 * t).sin()) * (-t * 22.0).exp() * 0.35));
    // Heal: soft warm double blip.
    bank.push(make(0.3, &mut |t, _| {
        let t2 = (t - 0.08).max(0.0);
        ((tau * 523.0 * t).sin() * (-t * 16.0).exp() + if t > 0.08 { (tau * 784.0 * t2).sin() * (-t2 * 12.0).exp() } else { 0.0 }) * 0.35
    }));
    // Note shot down: very short dry click.
    bank.push(make(0.03, &mut |t, n| (n * 0.3 + (tau * 2200.0 * t).sin() * 0.3) * (-t * 160.0).exp() * 0.4));
    // Track clear: gentle rising three-note figure.
    bank.push(make(0.9, &mut |t, _| {
        let mut s = 0.0;
        for (k, f) in [523.25f32, 659.25, 783.99].iter().enumerate() {
            let t0 = k as f32 * 0.12;
            if t > t0 {
                let x = t - t0;
                s += ((tau * f * x).sin() + 0.25 * (tau * 2.0 * f * x).sin()) * (-x * 5.0).exp();
            }
        }
        s * 0.3
    }));
    debug_assert_eq!(bank.len(), SFX_COUNT);
    bank
}

struct LiveState {
    running: bool,
    delayed: bool,
    ring: Vec<f32>,
    cap: usize,
    write_abs: u64,
    taken_abs: u64,
    read_pos: f64,
    cap_sr: u32,
    delay_frames: f64,
    rebuffering: bool,
    stamp: Instant,
    stamp_read: f64,
    latency: f64,
    last_write: Instant,
}

struct Shared {
    file: Mutex<FileState>,
    live: Mutex<LiveState>,
    volume: AtomicU32,
    sfx_volume: AtomicU32,
    sfx: Mutex<Vec<SfxVoice>>,
    devices: Mutex<Vec<DevInfo>>,
    status: Mutex<String>,
}

enum Cmd {
    EnsureFile,
    Refresh,
    StartLive { capture: Option<cpal::DeviceId>, playback: Option<cpal::DeviceId>, delay: f32 },
    StopLive,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LiveInfo {
    pub running: bool,
    pub delayed: bool,
    pub rebuffering: bool,
    pub buffered: f32,
    pub delay: f32,
    pub since_packet: f32,
    pub sr: u32,
}

pub struct Audio {
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    bank: Vec<Arc<Vec<f32>>>,
}

impl Audio {
    pub fn new() -> Audio {
        let now = Instant::now();
        let shared = Arc::new(Shared {
            file: Mutex::new(FileState {
                track: None,
                pos: 0.0,
                playing: false,
                started: false,
                finished: false,
                stamp: now,
                stamp_pos: 0.0,
                latency: 0.03,
                looping: false,
                gain: 1.0,
                gain_target: 1.0,
                gain_step: 1.0,
                stop_at_zero: false,
                prev: None,
            }),
            live: Mutex::new(LiveState {
                running: false,
                delayed: false,
                ring: Vec::new(),
                cap: 1,
                write_abs: 0,
                taken_abs: 0,
                read_pos: 0.0,
                cap_sr: 48_000,
                delay_frames: 0.0,
                rebuffering: true,
                stamp: now,
                stamp_read: 0.0,
                latency: 0.03,
                last_write: now,
            }),
            volume: AtomicU32::new(0.8f32.to_bits()),
            sfx_volume: AtomicU32::new(0.5f32.to_bits()),
            sfx: Mutex::new(Vec::with_capacity(MAX_SFX_VOICES)),
            devices: Mutex::new(Vec::new()),
            status: Mutex::new(String::new()),
        });
        let (tx, rx) = channel::<Cmd>();
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("heartline-audio".into())
            .spawn(move || {
                let host = cpal::default_host();
                refresh_devices(&host, &sh);
                let mut file_stream: Option<cpal::Stream> = None;
                let mut cap_stream: Option<cpal::Stream> = None;
                let mut live_out: Option<cpal::Stream> = None;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Refresh => refresh_devices(&host, &sh),
                        Cmd::EnsureFile => {
                            if file_stream.is_none() {
                                match host.default_output_device() {
                                    Some(dev) => match build_file_stream(&dev, sh.clone()) {
                                        Ok(s) => {
                                            let _ = s.play();
                                            file_stream = Some(s);
                                        }
                                        Err(e) => set_status(&sh, format!("audio output failed: {e}")),
                                    },
                                    None => set_status(&sh, "no audio output device".into()),
                                }
                            }
                        }
                        Cmd::StartLive { capture, playback, delay } => {
                            file_stream = None;
                            cap_stream = None;
                            live_out = None;
                            set_status(&sh, String::new());
                            let cap_dev = match &capture {
                                Some(id) => host.device_by_id(id),
                                None => host.default_output_device(),
                            };
                            let Some(cap_dev) = cap_dev else {
                                set_status(&sh, "capture device not found".into());
                                continue;
                            };
                            let cap_id = cap_dev.id().ok();
                            let out_dev = playback.as_ref().and_then(|id| host.device_by_id(id));
                            let out_id = out_dev.as_ref().and_then(|d| d.id().ok());
                            let delayed = out_dev.is_some() && out_id != cap_id && delay > 0.05;
                            match build_capture_stream(&cap_dev, sh.clone(), delay, delayed) {
                                Ok(s) => {
                                    let _ = s.play();
                                    cap_stream = Some(s);
                                }
                                Err(e) => {
                                    set_status(&sh, format!("loopback capture failed: {e}"));
                                    continue;
                                }
                            }
                            if delayed {
                                match build_live_out_stream(out_dev.as_ref().unwrap(), sh.clone()) {
                                    Ok(s) => {
                                        let _ = s.play();
                                        live_out = Some(s);
                                    }
                                    Err(e) => {
                                        set_status(&sh, format!("delayed playback failed ({e}); running reactive"));
                                        sh.live.lock().unwrap().delayed = false;
                                    }
                                }
                            }
                        }
                        Cmd::StopLive => {
                            cap_stream = None;
                            live_out = None;
                            sh.live.lock().unwrap().running = false;
                        }
                    }
                }
                drop((file_stream, cap_stream, live_out));
            })
            .expect("audio thread");
        Audio { shared, tx, bank: build_sfx_bank() }
    }

    // ---- sound effects ----
    pub fn set_sfx_volume(&self, v: f32) {
        self.shared.sfx_volume.store(v.clamp(0.0, 1.5).to_bits(), Ordering::Relaxed);
    }
    /// Make sure the output stream exists (so effects can play before any music has).
    pub fn ensure_output(&self) {
        let _ = self.tx.send(Cmd::EnsureFile);
    }
    /// Play a sound effect. `gain` 0..1, `pan` -1 (left) .. 1 (right).
    pub fn sfx(&self, s: Sfx, gain: f32, pan: f32) {
        let Some(buf) = self.bank.get(s as usize) else { return };
        let mut v = self.shared.sfx.lock().unwrap_or_else(|e| e.into_inner());
        if v.len() < MAX_SFX_VOICES {
            v.push(SfxVoice { buf: buf.clone(), pos: 0.0, gain: gain.clamp(0.0, 1.5), pan: pan.clamp(-1.0, 1.0) });
        }
    }

    // ---- common ----
    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.clamp(0.0, 1.5).to_bits(), Ordering::Relaxed);
    }
    pub fn status(&self) -> String {
        self.shared.status.lock().unwrap().clone()
    }
    pub fn devices(&self) -> Vec<DevInfo> {
        self.shared.devices.lock().unwrap().clone()
    }
    pub fn refresh_devices(&self) {
        let _ = self.tx.send(Cmd::Refresh);
    }

    // ---- file playback ----
    pub fn play(&self, track: Arc<Track>, from_secs: f64) {
        self.play_ex(track, from_secs, false, 0.0);
    }
    /// Start a track. With `fade > 0` it fades in over that many seconds while whatever was
    /// playing fades out underneath it (crossfade). `looping` wraps around at the end.
    pub fn play_ex(&self, track: Arc<Track>, from_secs: f64, looping: bool, fade: f32) {
        {
            let mut st = self.shared.file.lock().unwrap();
            let step = if fade > 0.01 { 1.0 / (fade * 48_000.0) } else { 1.0 };
            st.prev = match (st.track.take(), st.playing && fade > 0.01) {
                (Some(old), true) => Some((old, st.pos, st.gain, step)),
                _ => None,
            };
            st.pos = from_secs.max(0.0) * track.sr as f64;
            st.stamp_pos = st.pos;
            st.stamp = Instant::now();
            st.track = Some(track);
            st.playing = true;
            st.started = false;
            st.finished = false;
            st.looping = looping;
            st.gain = if fade > 0.01 { 0.0 } else { 1.0 };
            st.gain_target = 1.0;
            st.gain_step = step;
            st.stop_at_zero = false;
        }
        let _ = self.tx.send(Cmd::EnsureFile);
    }
    /// Fade the current track out over `secs` and then stop it.
    pub fn fade_out(&self, secs: f32) {
        let mut st = self.shared.file.lock().unwrap();
        st.gain_target = 0.0;
        st.gain_step = 1.0 / (secs.max(0.02) * 48_000.0);
        st.stop_at_zero = true;
    }
    /// Is a track loaded (playing or paused)?
    pub fn has_track(&self) -> bool {
        self.shared.file.lock().unwrap().track.is_some()
    }
    pub fn set_paused(&self, paused: bool) {
        let mut st = self.shared.file.lock().unwrap();
        st.playing = !paused;
        st.started = false;
        st.stamp_pos = st.pos;
        st.stamp = Instant::now();
    }
    pub fn stop(&self) {
        let mut st = self.shared.file.lock().unwrap();
        st.playing = false;
        st.track = None;
        st.prev = None;
        st.finished = false;
    }
    /// Position (seconds) of the audio that is audible right now.
    pub fn song_time(&self) -> f64 {
        let st = self.shared.file.lock().unwrap();
        let Some(tr) = &st.track else { return 0.0 };
        let sr = tr.sr as f64;
        let mut p = st.stamp_pos;
        if st.playing && st.started {
            p += st.stamp.elapsed().as_secs_f64().min(0.12) * sr;
        }
        (p / sr - st.latency).max(0.0)
    }
    pub fn finished(&self) -> bool {
        self.shared.file.lock().unwrap().finished
    }

    // ---- live ----
    pub fn start_live(&self, capture: Option<cpal::DeviceId>, playback: Option<cpal::DeviceId>, delay: f32) {
        let _ = self.tx.send(Cmd::StartLive { capture, playback, delay });
    }
    pub fn stop_live(&self) {
        let _ = self.tx.send(Cmd::StopLive);
    }
    pub fn live_info(&self) -> LiveInfo {
        let lv = self.shared.live.lock().unwrap();
        LiveInfo {
            running: lv.running,
            delayed: lv.delayed,
            rebuffering: lv.delayed && lv.rebuffering,
            buffered: ((lv.write_abs as f64 - lv.read_pos) / lv.cap_sr as f64) as f32,
            delay: (lv.delay_frames / lv.cap_sr as f64) as f32,
            since_packet: lv.last_write.elapsed().as_secs_f32(),
            sr: lv.cap_sr,
        }
    }
    /// Capture-timeline seconds of the audio that is audible right now.
    pub fn live_time(&self) -> f64 {
        let lv = self.shared.live.lock().unwrap();
        let sr = lv.cap_sr as f64;
        if lv.delayed {
            let mut p = lv.stamp_read;
            if !lv.rebuffering {
                p += lv.stamp.elapsed().as_secs_f64().min(0.12) * sr;
            }
            (p / sr - lv.latency).max(0.0)
        } else {
            (lv.write_abs as f64 + lv.last_write.elapsed().as_secs_f64().min(0.02) * sr) / sr
        }
    }
    /// Drain newly captured audio as mono samples (appended to `out`).
    pub fn live_take(&self, out: &mut Vec<f32>) {
        let mut lv = self.shared.live.lock().unwrap();
        if !lv.running || lv.cap < 2 {
            return;
        }
        let cap = lv.cap as u64;
        let oldest = lv.write_abs.saturating_sub(cap);
        if oldest > lv.taken_abs {
            // We fell far behind; keep the analysis timeline continuous with silence.
            let skipped = (oldest - lv.taken_abs).min(cap * 4) as usize;
            out.extend(std::iter::repeat(0.0).take(skipped));
            lv.taken_abs = oldest;
        }
        for a in lv.taken_abs..lv.write_abs {
            let i = (a % cap) as usize;
            out.push(0.5 * (lv.ring[2 * i] + lv.ring[2 * i + 1]));
        }
        lv.taken_abs = lv.write_abs;
    }
}

fn stream_error(sh: &Shared, what: &str, e: cpal::Error) {
    match e.kind() {
        cpal::ErrorKind::Xrun => {}
        _ => set_status(sh, format!("{what}: {e}")),
    }
}

fn set_status(sh: &Shared, s: String) {
    if !s.is_empty() {
        eprintln!("[audio] {s}");
    }
    *sh.status.lock().unwrap() = s;
}

fn refresh_devices(host: &cpal::Host, sh: &Shared) {
    let default_id = host.default_output_device().and_then(|d| d.id().ok());
    let mut list = Vec::new();
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            let Ok(id) = d.id() else { continue };
            let name = d
                .description()
                .map(|x| x.name().to_string())
                .unwrap_or_else(|_| id.to_string());
            let is_default = Some(&id) == default_id.as_ref();
            list.push(DevInfo { name, id, is_default });
        }
    }
    list.sort_by_key(|d| !d.is_default);
    *sh.devices.lock().unwrap() = list;
}

fn latency_of(info: &cpal::OutputCallbackInfo) -> f64 {
    let ts = info.timestamp();
    ts.playback
        .checked_duration_since(ts.callback)
        .unwrap_or(Duration::from_millis(30))
        .as_secs_f64()
        .clamp(0.0, 0.25)
}

// ---------------------------------------------------------------------------------------------
// File playback stream
// ---------------------------------------------------------------------------------------------

fn build_file_stream(dev: &cpal::Device, sh: Arc<Shared>) -> Result<cpal::Stream, String> {
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let config: StreamConfig = cfg.config();
    match cfg.sample_format() {
        SampleFormat::F32 => file_stream::<f32>(dev, config, sh),
        SampleFormat::I16 => file_stream::<i16>(dev, config, sh),
        SampleFormat::U16 => file_stream::<u16>(dev, config, sh),
        SampleFormat::I32 => file_stream::<i32>(dev, config, sh),
        SampleFormat::U8 => file_stream::<u8>(dev, config, sh),
        other => Err(format!("unsupported output sample format {other}")),
    }
}

fn file_stream<T>(dev: &cpal::Device, config: StreamConfig, sh: Arc<Shared>) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let ch = config.channels as usize;
    let out_sr = config.sample_rate as f64;
    let sh_err = sh.clone();
    // Scratch mix buffer (stereo), sized generously up front so the callback rarely grows it.
    let mut mix: Vec<f32> = vec![0.0; 2 * 8192];
    dev.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let vol = f32::from_bits(sh.volume.load(Ordering::Relaxed));
            let lat = latency_of(info);
            let mut st = sh.file.lock().unwrap_or_else(|e| e.into_inner());
            st.stamp = Instant::now();
            st.stamp_pos = st.pos;
            st.latency = lat;
            let frames = data.len() / ch.max(1);
            if mix.len() < frames * 2 {
                mix.resize(frames * 2, 0.0);
            }
            let mix = &mut mix[..frames * 2];
            for v in mix.iter_mut() {
                *v = 0.0;
            }
            let sample = |tr: &Track, pos: f64| -> (f32, f32) {
                let total = tr.frames();
                let i = pos as usize;
                if i + 1 < total {
                    let s = &tr.samples;
                    let fr = (pos - i as f64) as f32;
                    (s[2 * i] + (s[2 * i + 2] - s[2 * i]) * fr, s[2 * i + 1] + (s[2 * i + 3] - s[2 * i + 1]) * fr)
                } else {
                    (0.0, 0.0)
                }
            };
            // Fades are specified against 48 kHz; correct for the real device rate.
            let rate_k = (48_000.0 / out_sr) as f32;
            // Previous track fading out underneath (crossfade).
            if let Some((tr, mut pos, mut gain, gstep)) = st.prev.take() {
                let step = tr.sr as f64 / out_sr;
                let total = tr.frames();
                for f in 0..frames {
                    if gain <= 0.0 || pos as usize + 1 >= total {
                        gain = 0.0;
                        break;
                    }
                    let (l, r) = sample(&tr, pos);
                    mix[2 * f] += l * gain * vol;
                    mix[2 * f + 1] += r * gain * vol;
                    pos += step;
                    gain -= gstep * rate_k;
                }
                if gain > 0.0 {
                    st.prev = Some((tr, pos, gain, gstep));
                }
            }
            let track = if st.playing { st.track.clone() } else { None };
            if let Some(tr) = track {
                st.started = true;
                let step = tr.sr as f64 / out_sr;
                let total = tr.frames();
                let mut pos = st.pos;
                let mut gain = st.gain;
                let (target, gstep) = (st.gain_target, st.gain_step * rate_k);
                for f in 0..frames {
                    if st.looping && pos as usize + 1 >= total && total > 2 {
                        pos -= (total - 1) as f64;
                    }
                    let (l, r) = sample(&tr, pos);
                    mix[2 * f] += l * gain * vol;
                    mix[2 * f + 1] += r * gain * vol;
                    pos += step;
                    if gain < target {
                        gain = (gain + gstep).min(target);
                    } else if gain > target {
                        gain = (gain - gstep).max(target);
                    }
                }
                st.pos = pos;
                st.gain = gain;
                if !st.looping && pos as usize >= total {
                    st.finished = true;
                }
                if st.stop_at_zero && gain <= 0.0 {
                    st.playing = false;
                    st.track = None;
                    st.stop_at_zero = false;
                }
            }
            drop(st);
            // Sound effects on top.
            let svol = f32::from_bits(sh.sfx_volume.load(Ordering::Relaxed));
            {
                let mut voices = sh.sfx.lock().unwrap_or_else(|e| e.into_inner());
                let step = SFX_SR / out_sr;
                for v in voices.iter_mut() {
                    let (gl, gr) = (v.gain * svol * (1.0 - v.pan.max(0.0) * 0.6), v.gain * svol * (1.0 + v.pan.min(0.0) * 0.6));
                    let n = v.buf.len();
                    for f in 0..frames {
                        let i = v.pos as usize;
                        if i + 1 >= n {
                            v.pos = n as f64;
                            break;
                        }
                        let fr = (v.pos - i as f64) as f32;
                        let s = v.buf[i] + (v.buf[i + 1] - v.buf[i]) * fr;
                        mix[2 * f] += s * gl;
                        mix[2 * f + 1] += s * gr;
                        v.pos += step;
                    }
                }
                voices.retain(|v| (v.pos as usize) + 1 < v.buf.len());
            }
            for f in 0..frames {
                // Gentle soft clip so effects over a loud master never crackle.
                let (l, r) = (mix[2 * f], mix[2 * f + 1]);
                let clip = |x: f32| if x.abs() > 0.9 { x.signum() * (0.9 + 0.1 * ((x.abs() - 0.9) * 10.0).tanh()) } else { x };
                write_frame(&mut data[f * ch..(f + 1) * ch], clip(l), clip(r));
            }
        },
        move |e| stream_error(&sh_err, "output stream", e),
        None,
    )
    .map_err(|e| e.to_string())
}

#[inline]
fn write_frame<T: SizedSample + FromSample<f32>>(frame: &mut [T], l: f32, r: f32) {
    match frame.len() {
        0 => {}
        1 => frame[0] = T::from_sample(0.5 * (l + r)),
        _ => {
            frame[0] = T::from_sample(l);
            frame[1] = T::from_sample(r);
            for v in frame[2..].iter_mut() {
                *v = T::EQUILIBRIUM;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Live: loopback capture + delayed playback
// ---------------------------------------------------------------------------------------------

fn build_capture_stream(dev: &cpal::Device, sh: Arc<Shared>, delay: f32, delayed: bool) -> Result<cpal::Stream, String> {
    // The device is an *output* device; building an input stream on it enables WASAPI loopback.
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let config: StreamConfig = cfg.config();
    {
        let mut lv = sh.live.lock().unwrap();
        let sr = config.sample_rate;
        lv.cap_sr = sr;
        lv.cap = ((delay.max(0.0) + 6.0) * sr as f32) as usize;
        lv.ring = vec![0.0; lv.cap * 2];
        lv.write_abs = 0;
        lv.taken_abs = 0;
        lv.read_pos = 0.0;
        lv.stamp_read = 0.0;
        lv.delay_frames = (delay.max(0.0) * sr as f32) as f64;
        lv.delayed = delayed;
        lv.rebuffering = true;
        lv.running = true;
        lv.last_write = Instant::now();
        lv.stamp = Instant::now();
    }
    match cfg.sample_format() {
        SampleFormat::F32 => capture_stream::<f32>(dev, config, sh),
        SampleFormat::I16 => capture_stream::<i16>(dev, config, sh),
        SampleFormat::U16 => capture_stream::<u16>(dev, config, sh),
        SampleFormat::I32 => capture_stream::<i32>(dev, config, sh),
        other => Err(format!("unsupported capture sample format {other}")),
    }
}

fn capture_stream<T>(dev: &cpal::Device, config: StreamConfig, sh: Arc<Shared>) -> Result<cpal::Stream, String>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let ch = config.channels as usize;
    let sh_err = sh.clone();
    dev.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut lv = sh.live.lock().unwrap();
            let cap = lv.cap as u64;
            if cap < 2 || ch == 0 {
                return;
            }
            for fr in data.chunks_exact(ch) {
                let l = f32::from_sample(fr[0]);
                let r = if ch > 1 { f32::from_sample(fr[1]) } else { l };
                let i = (lv.write_abs % cap) as usize;
                lv.ring[2 * i] = l;
                lv.ring[2 * i + 1] = r;
                lv.write_abs += 1;
            }
            lv.last_write = Instant::now();
        },
        move |e| stream_error(&sh_err, "capture stream", e),
        None,
    )
    .map_err(|e| e.to_string())
}

fn build_live_out_stream(dev: &cpal::Device, sh: Arc<Shared>) -> Result<cpal::Stream, String> {
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let config: StreamConfig = cfg.config();
    match cfg.sample_format() {
        SampleFormat::F32 => live_out_stream::<f32>(dev, config, sh),
        SampleFormat::I16 => live_out_stream::<i16>(dev, config, sh),
        SampleFormat::U16 => live_out_stream::<u16>(dev, config, sh),
        SampleFormat::I32 => live_out_stream::<i32>(dev, config, sh),
        other => Err(format!("unsupported output sample format {other}")),
    }
}

fn live_out_stream<T>(dev: &cpal::Device, config: StreamConfig, sh: Arc<Shared>) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let ch = config.channels as usize;
    let out_sr = config.sample_rate as f64;
    let sh_err = sh.clone();
    dev.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let vol = f32::from_bits(sh.volume.load(Ordering::Relaxed));
            let lat = latency_of(info);
            let mut lv = sh.live.lock().unwrap();
            lv.stamp = Instant::now();
            lv.stamp_read = lv.read_pos;
            lv.latency = lat;
            let cap = lv.cap as u64;
            let sr = lv.cap_sr as f64;
            let base = sr / out_sr;
            let frames = data.len() / ch.max(1);
            if !lv.running || cap < 2 {
                for v in data.iter_mut() {
                    *v = T::EQUILIBRIUM;
                }
                return;
            }
            for f in 0..frames {
                let mut avail = lv.write_abs as f64 - lv.read_pos;
                if avail > cap as f64 - sr {
                    // Reader fell too far behind (would be overwritten): jump back to the target delay.
                    lv.read_pos = lv.write_abs as f64 - lv.delay_frames;
                    avail = lv.delay_frames;
                }
                if lv.rebuffering && avail >= lv.delay_frames {
                    lv.rebuffering = false;
                }
                if !lv.rebuffering && avail < 2.0 {
                    lv.rebuffering = true;
                }
                let (l, r) = if lv.rebuffering {
                    (0.0, 0.0)
                } else {
                    let i0 = lv.read_pos as u64;
                    let fr = (lv.read_pos - i0 as f64) as f32;
                    let a = (i0 % cap) as usize;
                    let b = ((i0 + 1) % cap) as usize;
                    let l = lv.ring[2 * a] + (lv.ring[2 * b] - lv.ring[2 * a]) * fr;
                    let r = lv.ring[2 * a + 1] + (lv.ring[2 * b + 1] - lv.ring[2 * a + 1]) * fr;
                    // Gentle drift correction keeps the delay near its target across device clocks.
                    let corr = ((avail - lv.delay_frames) / sr * 0.02).clamp(-0.004, 0.004);
                    lv.read_pos += base * (1.0 + corr);
                    (l, r)
                };
                write_frame(&mut data[f * ch..(f + 1) * ch], l * vol, r * vol);
            }
        },
        move |e| stream_error(&sh_err, "delayed output stream", e),
        None,
    )
    .map_err(|e| e.to_string())
}
