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
        Audio { shared, tx }
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
        {
            let mut st = self.shared.file.lock().unwrap();
            st.pos = from_secs * track.sr as f64;
            st.stamp_pos = st.pos;
            st.stamp = Instant::now();
            st.track = Some(track);
            st.playing = true;
            st.started = false;
            st.finished = false;
        }
        let _ = self.tx.send(Cmd::EnsureFile);
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
    dev.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let vol = f32::from_bits(sh.volume.load(Ordering::Relaxed));
            let lat = latency_of(info);
            let mut st = sh.file.lock().unwrap();
            st.stamp = Instant::now();
            st.stamp_pos = st.pos;
            st.latency = lat;
            let frames = data.len() / ch.max(1);
            let track = if st.playing { st.track.clone() } else { None };
            match track {
                Some(tr) => {
                    st.started = true;
                    let step = tr.sr as f64 / out_sr;
                    let total = tr.frames();
                    let s = &tr.samples;
                    let mut pos = st.pos;
                    for f in 0..frames {
                        let i = pos as usize;
                        let (l, r) = if i + 1 < total {
                            let fr = (pos - i as f64) as f32;
                            (
                                s[2 * i] + (s[2 * i + 2] - s[2 * i]) * fr,
                                s[2 * i + 1] + (s[2 * i + 3] - s[2 * i + 1]) * fr,
                            )
                        } else {
                            (0.0, 0.0)
                        };
                        write_frame(&mut data[f * ch..(f + 1) * ch], l * vol, r * vol);
                        pos += step;
                    }
                    st.pos = pos;
                    if pos as usize >= total {
                        st.finished = true;
                    }
                }
                None => {
                    for v in data.iter_mut() {
                        *v = T::EQUILIBRIUM;
                    }
                }
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
