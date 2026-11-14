//! Heartline — your music becomes a top-down vaporwave bullet-hell.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analysis;
mod decode;
mod engine;
mod game;
mod level;
mod render;
mod rng;
mod synth;
mod tools;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use macroquad::prelude::*;

use analysis::{Analysis, Band, Feat, SectionKind, StreamAnalyzer, StreamOut};
use decode::Track;
use engine::{Audio, Sfx};
use game::{Input, UpgradeId, World};
use level::{Ev, Level, LevelEvent, LiveGen, Timeline};
use render::{text_center, text_left, text_shadow, text_w, HudInfo, Renderer};

// ---------------------------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------------------------

#[derive(Default, Clone)]
pub struct Args {
    files: Vec<PathBuf>,
    analyze: Option<PathBuf>,
    gen_test: Option<PathBuf>,
    live_probe: Option<f32>,
    selftest: Option<PathBuf>,
    shots: Vec<f32>,
    secs: f32,
    start: f32,
    seed: u64,
    ss: Option<u32>,
    diff: usize,
    mute: bool,
    size: Option<(i32, i32)>,
    live: bool,
    give: u32,
    wall: bool,
    mortal: bool,
    /// Debug: keep the drift (idle) scene up while the song plays, with no attacks.
    drift: bool,
    /// Headless balance simulation over `files`.
    sim: bool,
    sample: usize,
    runs: u32,
    bot_delay: f32,
    song_idx: u32,
    diff_set: bool,
    screen: String,
    live_in: String,
    live_out: String,
}

fn parse_args() -> Args {
    let mut a = Args { diff: 1, secs: 30.0, runs: 1, bot_delay: 140.0, ..Default::default() };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().unwrap_or_default();
        match arg.as_str() {
            "--analyze" => a.analyze = Some(PathBuf::from(val())),
            "--gen-test" => a.gen_test = Some(PathBuf::from(val())),
            "--live-probe" => a.live_probe = Some(val().parse().unwrap_or(4.0)),
            "--selftest" => a.selftest = Some(PathBuf::from(val())),
            "--shots" => a.shots = val().split(',').filter_map(|s| s.trim().parse().ok()).collect(),
            "--secs" => a.secs = val().parse().unwrap_or(30.0),
            "--start" => a.start = val().parse().unwrap_or(0.0),
            "--seed" => a.seed = val().parse().unwrap_or(0),
            "--ss" => a.ss = val().parse().ok(),
            "--give" => a.give = val().parse().unwrap_or(0),
            "--sim" => a.sim = true,
            "--sample" => a.sample = val().parse().unwrap_or(0),
            "--runs" => a.runs = val().parse().unwrap_or(1),
            "--bot-delay" => a.bot_delay = val().parse().unwrap_or(140.0),
            "--song-idx" => a.song_idx = val().parse().unwrap_or(0),
            "--diff" => {
                a.diff_set = true;
                a.diff = match val().to_ascii_lowercase().as_str() {
                    "chill" | "0" => 0,
                    "hard" | "2" => 2,
                    _ => 1,
                }
            }
            "--mute" => a.mute = true,
            "--wall" => a.wall = true,
            "--mortal" => a.mortal = true,
            "--drift" => a.drift = true,
            "--live-in" => {
                a.live_in = val();
                a.live = true;
            }
            "--live-out" => {
                a.live_out = val();
                a.live = true;
            }
            "--screen" => a.screen = val(),
            "--live" => a.live = true,
            "--size" => {
                let v = val();
                let mut p = v.split('x');
                if let (Some(w), Some(h)) = (p.next().and_then(|s| s.parse().ok()), p.next().and_then(|s| s.parse().ok())) {
                    a.size = Some((w, h));
                }
            }
            _ => {
                let p = PathBuf::from(&arg);
                if p.exists() {
                    a.files.extend(expand_paths(&[p]));
                }
            }
        }
    }
    a
}

/// Expand folders into the audio files they contain (sorted, recursive, capped).
fn expand_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
        if depth > 4 || out.len() > 2000 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, out, depth + 1);
            } else if decode::is_audio_file(&p) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            walk(p, &mut out, 0);
        } else if decode::is_audio_file(p) {
            out.push(p.clone());
        }
    }
    out
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn AttachConsole(process_id: u32) -> i32;
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("Heartline crashed:\n{info}\n");
        eprintln!("{msg}");
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::fs::write(exe.with_file_name("heartline-crash.log"), &msg);
        }
    }));
    let args = parse_args();
    let cli = args.analyze.is_some() || args.gen_test.is_some() || args.live_probe.is_some() || args.sim;
    if cli {
        #[cfg(windows)]
        unsafe {
            AttachConsole(u32::MAX);
        }
        tools::run_cli(&args);
        return;
    }
    let (w, h) = args.size.unwrap_or((1600, 900));
    let conf = Conf {
        window_title: "Heartline".to_owned(),
        window_width: w,
        window_height: h,
        high_dpi: true,
        window_resizable: true,
        sample_count: 1,
        ..Default::default()
    };
    macroquad::Window::from_config(conf, run(args));
}

// ---------------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------------

pub struct Loaded {
    pub track: Arc<Track>,
    pub an: Arc<Analysis>,
    pub level: Level,
}

struct Loader {
    idx: usize,
    rx: Receiver<Result<Loaded, String>>,
    progress: Arc<AtomicU32>,
    stage: Arc<AtomicU32>,
}

pub fn load_track(path: &Path, seed: u64, diff: f32, progress: &AtomicU32, stage: &AtomicU32) -> Result<Loaded, String> {
    stage.store(0, Ordering::Relaxed);
    let track = match theme_index(path) {
        Some(i) => synth::theme(i),
        None => decode::decode_file(path, progress)?,
    };
    stage.store(1, Ordering::Relaxed);
    progress.store(0, Ordering::Relaxed);
    let mono = track.mono();
    let an = analysis::analyze(&mono, track.sr, progress);
    let level = level::generate(&an, seed, diff);
    Ok(Loaded { track: Arc::new(track), an: Arc::new(an), level })
}

fn spawn_loader(idx: usize, path: PathBuf, seed: u64, diff: f32) -> Loader {
    let (tx, rx) = channel();
    let progress = Arc::new(AtomicU32::new(0));
    let stage = Arc::new(AtomicU32::new(0));
    let (p2, s2) = (progress.clone(), stage.clone());
    std::thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_track(&path, seed, diff, &p2, &s2)))
            .unwrap_or_else(|_| Err("analysis crashed".to_string()));
        let _ = tx.send(res);
    });
    Loader { idx, rx, progress, stage }
}

// ---------------------------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Menu,
    LiveSetup,
    Play,
    Paused,
    Results,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Loading,
    Song,
    Intermission(f32),
    Dying(f32),
}

struct Song {
    track: Arc<Track>,
    an: Arc<Analysis>,
    level: Level,
    timeline: Arc<Timeline>,
    cursor: usize,
    last_t: f32,
}

struct LiveRun {
    an: Option<StreamAnalyzer>,
    gen: LiveGen,
    feats: VecDeque<(f32, Feat, f32)>,
    pending: Vec<LevelEvent>,
    buf: Vec<f32>,
    out: StreamOut,
    silent_for: f32,
    was_idle: bool,
    last_beat_t: f32,
    beat_n: u32,
    last_clock: f32,
    delay: f32,
    hearts: u32,
    /// Estimated seconds per beat (median of recent kick-to-kick gaps).
    period: f32,
    ibis: VecDeque<f32>,
}

const DIFFS: [(f32, &str); 3] = [(0.65, "CHILL  (no-fail)"), (1.0, "NORMAL"), (1.35, "HARD")];

struct App {
    args: Args,
    audio: Audio,
    rend: Renderer,
    world: World,
    screen: Screen,
    phase: Phase,
    queue: Vec<PathBuf>,
    qi: usize,
    loader: Option<Loader>,
    ready: Option<(usize, Loaded)>,
    song: Option<Song>,
    live: Option<LiveRun>,
    idle: f32,
    diff: usize,
    volume: f32,
    sfx_volume: f32,
    /// Extra audio latency in ms (soundbars, Bluetooth): events are delayed by this much.
    offset_ms: f32,
    music_dir: Option<PathBuf>,
    juke: Jukebox,
    autofire: bool,
    fullscreen: bool,
    menu_sel: usize,
    dialog: Option<Receiver<Vec<PathBuf>>>,
    /// True when the open folder dialog is choosing the music library (not starting a run).
    dialog_library: bool,
    /// Escape / P was consumed this frame (so pausing does not instantly un-pause).
    esc_used: bool,
    message: String,
    message_t: f32,
    died: bool,
    // live setup
    cap_sel: usize,
    out_sel: usize,
    delay: f32,
    // selftest
    shots_done: usize,
    frames: u64,
    wall: f32,
    max_bullets: usize,
    max_enemies: usize,
    log: Vec<String>,
}

/// Built-in synthesised tracks travel through playlists as pseudo paths ("::theme0").
fn theme_path(i: usize) -> PathBuf {
    PathBuf::from(format!("::theme{i}"))
}

fn theme_index(p: &Path) -> Option<usize> {
    p.to_str()?.strip_prefix("::theme")?.parse().ok()
}

/// Where the menu starts previewing a track: a bar before its first big section.
fn preview_start(an: &Analysis) -> f32 {
    let beat = if an.bpm > 1.0 { 60.0 / an.bpm } else { 0.5 };
    an.sections
        .iter()
        .find(|s| matches!(s.kind, SectionKind::Intense | SectionKind::Drop) && s.t0 > 1.0 && s.t0 < an.duration * 0.7)
        .map(|s| (s.t0 - 4.0 * beat).max(0.0))
        .unwrap_or(0.0)
}

/// The menu's music: plays through the library (or the built-in tracks), fully analysed so
/// the menu scene reacts to it and so pressing PLAY starts instantly.
struct Jukebox {
    list: Vec<PathBuf>,
    idx: usize,
    loaded: Option<(PathBuf, Loaded)>,
    loader: Option<(PathBuf, Loader)>,
    cursor: usize,
    last_t: f32,
    playing: bool,
    fails: u32,
}

// ---------------------------------------------------------------------------------------------
// Settings (heartline.cfg next to the exe)
// ---------------------------------------------------------------------------------------------

fn settings_path() -> Option<PathBuf> {
    std::env::current_exe().ok().map(|e| e.with_file_name("heartline.cfg"))
}

fn load_settings() -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    if let Some(text) = settings_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                m.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    m
}

/// The user's Music folder (honours a relocated library).
#[cfg(windows)]
fn music_folder() -> Option<PathBuf> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Guid(u32, u16, u16, [u8; 8]);
    #[link(name = "shell32")]
    extern "system" {
        fn SHGetKnownFolderPath(rfid: *const Guid, flags: u32, token: *mut c_void, path: *mut *mut u16) -> i32;
    }
    #[link(name = "ole32")]
    extern "system" {
        fn CoTaskMemFree(p: *mut c_void);
    }
    // FOLDERID_Music {4BD8D571-6D19-48D3-BE97-422220080E43}
    let id = Guid(0x4BD8_D571, 0x6D19, 0x48D3, [0xBE, 0x97, 0x42, 0x22, 0x20, 0x08, 0x0E, 0x43]);
    let mut raw: *mut u16 = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(&id, 0, std::ptr::null_mut(), &mut raw) };
    if hr != 0 || raw.is_null() {
        return None;
    }
    let mut len = 0usize;
    unsafe {
        while *raw.add(len) != 0 && len < 4096 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(raw, len));
        CoTaskMemFree(raw as *mut c_void);
        Some(PathBuf::from(s))
    }
}

#[cfg(not(windows))]
fn music_folder() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Music"))
}

fn track_title(p: &Path) -> String {
    if let Some(i) = theme_index(p) {
        return synth::theme_name(i).to_string();
    }
    p.file_stem().and_then(|s| s.to_str()).unwrap_or("untitled").to_string()
}

fn feat_at(an: &Analysis, t: f32) -> Feat {
    if an.feats.is_empty() {
        return Feat::default();
    }
    let x = (t / an.hop_dt).max(0.0);
    let i = (x as usize).min(an.feats.len() - 1);
    let j = (i + 1).min(an.feats.len() - 1);
    Feat::lerp(&an.feats[i], &an.feats[j], x.fract())
}

fn beat_pos(beats: &[f32], t: f32) -> f32 {
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

impl App {
    fn diff(&self) -> f32 {
        DIFFS[self.diff].0
    }

    fn say(&mut self, s: impl Into<String>) {
        self.message = s.into();
        self.message_t = 5.0;
    }

    fn save_settings(&self) {
        if self.args.selftest.is_some() {
            return;
        }
        let Some(path) = settings_path() else { return };
        let mut text = format!(
            "diff={}\nvolume={:.2}\nsfx={:.2}\noffset_ms={:.0}\nautofire={}\n",
            self.diff, self.volume, self.sfx_volume, self.offset_ms, self.autofire as u8
        );
        if let Some(d) = &self.music_dir {
            text.push_str(&format!("music_dir={}\n", d.display()));
        }
        let _ = std::fs::write(path, text);
    }

    /// Rebuild the menu playlist: the library (shuffled), then the built-in tracks.
    fn juke_rescan(&mut self) {
        let mut lib = self.music_dir.as_ref().map(|d| expand_paths(&[d.clone()])).unwrap_or_default();
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        let mut rng = rng::Rng::new(if self.args.selftest.is_some() { 7 } else { seed });
        for i in (1..lib.len()).rev() {
            lib.swap(i, rng.below(i + 1));
        }
        let first_theme = rng.below(synth::THEME_COUNT);
        lib.extend((0..synth::THEME_COUNT).map(|k| theme_path((first_theme + k) % synth::THEME_COUNT)));
        self.juke = Jukebox { list: lib, idx: 0, loaded: None, loader: None, cursor: 0, last_t: -1.0, playing: false, fails: 0 };
    }

    fn juke_next(&mut self, dir: i32) {
        let n = self.juke.list.len() as i32;
        if n > 1 {
            self.juke.idx = (self.juke.idx as i32 + dir).rem_euclid(n) as usize;
            self.juke.loader = None;
            self.juke.fails = 0;
            self.audio.sfx(Sfx::UiMove, 0.8, 0.0);
        }
    }

    /// Menu-like screens: keep the menu music loaded and playing, and drive the world's music
    /// signals from it so the scene is a visualiser. Returns progress through the track.
    fn juke_step(&mut self, dt: f32, input: &Input) -> f32 {
        let mut done = None;
        if let Some((path, l)) = &self.juke.loader {
            if let Ok(res) = l.rx.try_recv() {
                done = Some((path.clone(), res));
            }
        }
        if let Some((path, res)) = done {
            self.juke.loader = None;
            match res {
                Ok(loaded) => {
                    if self.juke.list.get(self.juke.idx) == Some(&path) {
                        self.juke.loaded = Some((path, loaded));
                        self.juke.playing = false;
                        self.juke.fails = 0;
                    }
                }
                Err(_) => {
                    self.juke.fails += 1;
                    self.juke.list.retain(|p| *p != path);
                    if self.juke.idx >= self.juke.list.len() {
                        self.juke.idx = 0;
                    }
                }
            }
        }
        let n = self.juke.list.len();
        if n > 0 && self.juke.loader.is_none() && self.juke.fails < 6 {
            let want = self.juke.list[self.juke.idx % n].clone();
            if self.juke.loaded.as_ref().map(|l| &l.0) != Some(&want) {
                let l = spawn_loader(0, want.clone(), self.args.seed, self.diff());
                self.juke.loader = Some((want, l));
            }
        }
        if !self.juke.playing {
            if let Some((path, loaded)) = &self.juke.loaded {
                let start = if theme_index(path).is_some() { 0.0 } else { preview_start(&loaded.an) };
                self.audio.play_ex(loaded.track.clone(), start as f64, true, 0.9);
                self.juke.playing = true;
                self.juke.last_t = -1.0;
                self.world.warp = 1.0;
                self.world.beat_len = if loaded.level.bpm > 1.0 { 60.0 / loaded.level.bpm } else { 0.5 };
            }
        }
        let mut feat = Feat::default();
        let mut bp = self.rend.time * 100.0 / 60.0;
        let mut intensity = 0.0;
        let mut progress = 0.0;
        if self.juke.playing {
            if let Some((_, loaded)) = &self.juke.loaded {
                let t = self.audio.song_time() as f32;
                let ev = &loaded.level.events;
                if self.juke.last_t < 0.0 || t < self.juke.last_t - 0.3 {
                    // Started or looped: find our place and restore the section look.
                    self.juke.cursor = ev.partition_point(|e| e.at() <= t);
                    if let Some(e) = ev[..self.juke.cursor].iter().rev().find(|e| matches!(e.ev, Ev::Section { .. })) {
                        self.world.apply(e);
                    }
                }
                self.juke.last_t = t;
                self.world.t = t;
                while self.juke.cursor < ev.len() && ev[self.juke.cursor].at() <= t {
                    let e = ev[self.juke.cursor];
                    self.juke.cursor += 1;
                    if matches!(e.ev, Ev::Beat { .. } | Ev::Hit { .. } | Ev::Section { .. }) {
                        self.world.apply(&e);
                    }
                }
                feat = feat_at(&loaded.an, t);
                bp = beat_pos(&loaded.level.beats, t);
                intensity = loaded.an.intensity_at(t);
                progress = t / loaded.level.duration.max(0.1);
            }
        }
        let t = self.world.t;
        self.world.update(dt, t, input, bp, &feat, intensity, false);
        progress
    }

    /// Start a run with the track the menu is playing, plus `extra` more from the playlist.
    fn start_from_juke(&mut self, extra: usize) {
        let n = self.juke.list.len();
        if n == 0 {
            self.say("no music found - choose a MUSIC LIBRARY folder or drop files on the window");
            return;
        }
        let mut paths = vec![self.juke.list[self.juke.idx % n].clone()];
        for k in 1..=extra {
            let p = self.juke.list[(self.juke.idx + k) % n].clone();
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        // The menu already analysed this track: reuse it so the level starts instantly.
        let reuse = self.juke.loaded.as_ref().filter(|(p, _)| *p == paths[0]).map(|(_, l)| (l.track.clone(), l.an.clone()));
        self.start_run(paths);
        if let Some((track, an)) = reuse {
            let level = level::generate(&an, self.args.seed, self.diff());
            self.loader = None;
            self.ready = Some((0, Loaded { track, an, level }));
        }
    }

    fn to_menu(&mut self) {
        self.juke.playing = false;
        self.audio.stop();
        self.audio.stop_live();
        self.song = None;
        self.live = None;
        self.loader = None;
        self.ready = None;
        self.screen = Screen::Menu;
        self.world = World::new(self.diff(), self.diff == 0);
        self.rend.set_palette(0);
    }

    fn start_run(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.say("no playable audio files found");
            return;
        }
        // Menu music keeps playing through the loading screen and cross-fades into the level.
        if !self.juke.playing {
            self.audio.stop();
        }
        self.juke.playing = false;
        self.audio.stop_live();
        self.live = None;
        self.song = None;
        self.ready = None;
        self.queue = paths;
        self.qi = 0;
        let god = self.args.selftest.is_some() && !self.args.mortal;
        self.world = World::new(self.diff(), self.diff == 0 || god);
        for _ in 0..self.args.give {
            let up = UpgradeId::ALL[self.world.rng.below(UpgradeId::ALL.len())];
            self.world.player.ups[up as usize] = (self.world.player.ups[up as usize] + 1).min(up.max());
        }
        self.died = false;
        self.loader = Some(spawn_loader(0, self.queue[0].clone(), self.args.seed, self.diff()));
        self.screen = Screen::Play;
        self.phase = Phase::Loading;
    }

    fn start_live(&mut self) {
        let devs = self.audio.devices();
        let capture = devs.get(self.cap_sel).map(|d| d.id.clone());
        // out_sel 0 = none (reactive)
        let playback = if self.out_sel == 0 { None } else { devs.get(self.out_sel - 1).map(|d| d.id.clone()) };
        let delayed = playback.is_some() && playback != capture;
        let delay = if delayed { self.delay } else { 0.0 };
        self.juke.playing = false;
        self.audio.stop();
        self.audio.start_live(capture, playback, delay);
        self.song = None;
        self.world = World::new(self.diff(), self.diff == 0);
        self.world.begin_song(210.0, 0, 0xC0FFEE);
        let lookahead = if delayed { (delay - 0.25).max(0.0) } else { 0.0 };
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(7);
        self.live = Some(LiveRun {
            an: None,
            gen: LiveGen::new(seed ^ self.args.seed, self.diff(), lookahead),
            feats: VecDeque::new(),
            pending: Vec::new(),
            buf: Vec::new(),
            out: StreamOut::default(),
            silent_for: 10.0,
            was_idle: true,
            last_beat_t: -1.0,
            beat_n: 0,
            last_clock: 0.0,
            delay,
            hearts: 0,
            period: 0.5,
            ibis: VecDeque::new(),
        });
        self.died = false;
        self.screen = Screen::Play;
        self.phase = Phase::Song;
    }

    fn poll_loader(&mut self) {
        let mut done = None;
        if let Some(l) = &self.loader {
            if let Ok(res) = l.rx.try_recv() {
                done = Some((l.idx, res));
            }
        }
        if let Some((idx, res)) = done {
            self.loader = None;
            match res {
                Ok(loaded) => self.ready = Some((idx, loaded)),
                Err(e) => {
                    let name = self.queue.get(idx).map(|p| track_title(p)).unwrap_or_default();
                    self.say(format!("skipped {name}: {e}"));
                    if idx < self.queue.len() {
                        self.queue.remove(idx);
                    }
                    if self.queue.is_empty() || (self.qi >= self.queue.len() && self.song.is_none()) {
                        if self.world.stats.songs > 0 {
                            self.screen = Screen::Results;
                        } else {
                            self.to_menu();
                        }
                    }
                }
            }
        }
        // Keep the next track warming up in the background.
        if self.loader.is_none() && self.screen != Screen::Menu && self.live.is_none() {
            let want = if self.song.is_some() && self.phase == Phase::Song { self.qi + 1 } else { self.qi };
            let have = self.ready.as_ref().map(|r| r.0);
            if want < self.queue.len() && have != Some(want) {
                self.loader = Some(spawn_loader(want, self.queue[want].clone(), self.args.seed, self.diff()));
            }
        }
    }

    fn begin_song(&mut self, loaded: Loaded) {
        let start = if self.qi == 0 { self.args.start.max(0.0) } else { 0.0 };
        self.world.begin_song(loaded.level.active_secs, self.qi as u32, loaded.level.seed);
        self.world.beat_len = if loaded.level.bpm > 1.0 { 60.0 / loaded.level.bpm } else { 0.5 };
        let timeline = Arc::new(Timeline::from_analysis(&loaded.an));
        let mut song = Song { track: loaded.track, an: loaded.an, level: loaded.level, timeline, cursor: 0, last_t: start };
        if start > 0.0 {
            // Skip what is already in the past, but keep the latest section so the look is right.
            let mut last_section = None;
            while song.cursor < song.level.events.len() && song.level.events[song.cursor].at() < start {
                if matches!(song.level.events[song.cursor].ev, Ev::Section { .. }) {
                    last_section = Some(song.level.events[song.cursor]);
                }
                song.cursor += 1;
            }
            if let Some(e) = last_section {
                self.world.t = start;
                self.world.apply(&e);
            }
        }
        self.audio.set_volume(if self.args.mute { 0.0 } else { self.volume });
        let fade = if self.audio.has_track() { 0.45 } else { 0.0 };
        self.audio.play_ex(song.track.clone(), start as f64, false, fade);
        self.juke.playing = false;
        self.song = Some(song);
        self.phase = Phase::Song;
    }

    fn gather_input(&self, allow_fire: bool) -> Input {
        let mut mv = Vec2::ZERO;
        if is_key_down(KeyCode::A) || is_key_down(KeyCode::Left) {
            mv.x -= 1.0;
        }
        if is_key_down(KeyCode::D) || is_key_down(KeyCode::Right) {
            mv.x += 1.0;
        }
        if is_key_down(KeyCode::W) || is_key_down(KeyCode::Up) {
            mv.y -= 1.0;
        }
        if is_key_down(KeyCode::S) || is_key_down(KeyCode::Down) {
            mv.y += 1.0;
        }
        let (mx, my) = mouse_position();
        let aim = self.rend.view.to_world(vec2(mx, my));
        Input {
            mv,
            aim,
            fire: allow_fire && (self.autofire || is_mouse_button_down(MouseButton::Left)),
            dash: is_key_pressed(KeyCode::Space) || is_key_pressed(KeyCode::LeftShift) || is_mouse_button_pressed(MouseButton::Right),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Per-frame simulation for each mode. Returns (idle_target, hud).
    // -----------------------------------------------------------------------------------------

    fn step_files(&mut self, dt: f32, input: &Input) -> (f32, HudInfo, f32) {
        let mut hud = HudInfo::empty();
        let total = self.queue.len();
        if total > 1 {
            hud.track_no = format!("{}/{}", (self.qi + 1).min(total), total);
        }
        let idle_beat = self.rend.time * 100.0 / 60.0;
        match self.phase {
            Phase::Loading => {
                if let Some((idx, _)) = &self.ready {
                    if *idx == self.qi {
                        let (_, loaded) = self.ready.take().unwrap();
                        self.begin_song(loaded);
                        return self.step_files(dt, input);
                    }
                }
                let name = self.queue.get(self.qi).map(|p| track_title(p)).unwrap_or_default();
                hud.title = name;
                if let Some(l) = &self.loader {
                    let p = l.progress.load(Ordering::Relaxed) as f32 / 10.0;
                    let stage = if l.stage.load(Ordering::Relaxed) == 0 { "DECODING" } else { "READING THE MUSIC" };
                    hud.status = format!("{stage}  {p:.0}%");
                } else {
                    hud.status = "PREPARING".into();
                }
                let t = self.world.t;
                let mut inp = *input;
                inp.fire = false;
                self.world.update(dt, t, &inp, idle_beat, &Feat::default(), 0.0, false);
                (1.0, hud, 0.0)
            }
            Phase::Song => {
                let Some(song) = self.song.as_mut() else {
                    self.phase = Phase::Loading;
                    return (1.0, hud, 0.0);
                };
                let mut t = (self.audio.song_time() as f32 - self.offset_ms / 1000.0).max(0.0);
                if t < song.last_t {
                    t = song.last_t;
                }
                // Never let a hitch fast-forward the simulation by more than a moment.
                t = t.min(song.last_t + 0.25);
                song.last_t = t;
                self.world.t = t;
                let drift = self.args.drift;
                while song.cursor < song.level.events.len() && song.level.events[song.cursor].at() <= t {
                    let e = song.level.events[song.cursor];
                    song.cursor += 1;
                    if !drift || matches!(e.ev, Ev::Beat { .. } | Ev::Hit { .. } | Ev::Section { .. } | Ev::End) {
                        self.world.apply(&e);
                    }
                }
                let silent = drift || (song.an.in_silence(t) && song.an.in_silence(t + 0.7));
                let feat = feat_at(&song.an, t);
                let intensity = song.an.intensity_at(t);
                let bp = beat_pos(&song.level.beats, t);
                self.world.update(dt, t, input, bp, &feat, intensity, !silent);
                let dur = song.level.duration.max(0.1);
                hud.title = song.track.name.clone();
                hud.progress = t / dur;
                hud.time_left = dur - t;
                hud.bpm = song.level.bpm;
                hud.show_combat = !drift;
                hud.timeline = Some(song.timeline.clone());
                let finished = self.world.ended || (self.audio.finished() && t >= dur - 0.3);
                if self.world.dead {
                    self.phase = Phase::Dying(0.0);
                    self.audio.set_volume(if self.args.mute { 0.0 } else { self.volume * 0.35 });
                } else if finished {
                    if !self.world.ended {
                        self.world.apply(&LevelEvent { t, lead: 0.0, ev: Ev::End });
                    }
                    self.audio.stop();
                    self.world.calm_down();
                    self.qi += 1;
                    self.song = None;
                    if self.qi < self.queue.len() {
                        self.world.spawn_orbs(22.0);
                        self.phase = Phase::Intermission(0.0);
                    } else {
                        self.screen = Screen::Results;
                    }
                }
                (if silent { 1.0 } else { 0.0 }, hud, hud_progress(t, dur))
            }
            Phase::Intermission(mut it) => {
                it += dt;
                self.phase = Phase::Intermission(it);
                let t = self.world.t;
                let mut inp = *input;
                inp.fire = false;
                self.world.update(dt, t, &inp, idle_beat, &Feat::default(), 0.0, false);
                hud.title = self.queue.get(self.qi).map(|p| format!("NEXT  {}", track_title(p))).unwrap_or_default();
                hud.banner = "TRACK CLEAR".into();
                hud.banner_a = (it * 3.0).min(1.0) * ((2.2 - it) * 2.5).clamp(0.0, 1.0);
                let ready = self.ready.as_ref().map(|r| r.0 == self.qi).unwrap_or(false);
                hud.status = if !self.world.orbs.is_empty() {
                    "FLY INTO AN UPGRADE".into()
                } else if !ready {
                    "READING THE MUSIC".into()
                } else {
                    String::new()
                };
                // The next track rolls in after a short breath; upgrade orbs stay on the field
                // through its opening seconds (nothing attacks yet).
                if ready && it > 2.0 {
                    let (_, loaded) = self.ready.take().unwrap();
                    self.begin_song(loaded);
                }
                (1.0, hud, 0.0)
            }
            Phase::Dying(mut dtm) => {
                dtm += dt;
                self.phase = Phase::Dying(dtm);
                let t = self.world.t;
                self.world.update(dt * 0.35, t, input, 0.0, &Feat::default(), 0.0, false);
                if dtm > 1.8 {
                    self.audio.stop();
                    self.audio.stop_live();
                    self.died = true;
                    self.screen = Screen::Results;
                }
                hud.show_combat = true;
                (0.0, hud, 0.0)
            }
        }
    }

    fn step_live(&mut self, dt: f32, input: &Input) -> (f32, HudInfo, f32) {
        let mut hud = HudInfo::empty();
        hud.title = "LIVE AUDIO".into();
        hud.show_combat = true;
        if let Phase::Dying(mut dtm) = self.phase {
            dtm += dt;
            self.phase = Phase::Dying(dtm);
            let t = self.world.t;
            self.world.update(dt * 0.35, t, input, 0.0, &Feat::default(), 0.0, false);
            if dtm > 1.8 {
                self.audio.stop_live();
                self.died = true;
                self.live = None;
                self.screen = Screen::Results;
            }
            return (0.0, hud, 0.0);
        }
        let info = self.audio.live_info();
        let status = self.audio.status();
        let Some(live) = self.live.as_mut() else { return (1.0, hud, 0.0) };
        if !info.running {
            hud.status = if status.is_empty() { "OPENING AUDIO DEVICE".into() } else { status };
            let t = self.world.t;
            self.world.update(dt, t, input, self.rend.time * 100.0 / 60.0, &Feat::default(), 0.0, false);
            return (1.0, hud, 0.0);
        }
        let an = live.an.get_or_insert_with(|| StreamAnalyzer::new(info.sr));

        // Analyse whatever was captured since the last frame.
        live.buf.clear();
        self.audio.live_take(&mut live.buf);
        live.out.feats.clear();
        live.out.onsets.clear();
        an.push(&live.buf, &mut live.out);
        let live_beat = an.live_beat();
        let mut fresh: Vec<LevelEvent> = Vec::new();
        for &(ft, f, raw) in &live.out.feats {
            live.gen.feed(ft, f.rms, &mut fresh);
            live.feats.push_back((ft, f, raw));
        }
        for o in &live.out.onsets {
            live.gen.on_onset(o, &mut fresh);
            if o.band == Band::Low && o.strength > 0.3 && o.t - live.last_beat_t > 0.28 {
                // Fold the gap into the 0.33..0.9 s range (about 67..180 BPM) and keep a running median.
                let mut gap = o.t - live.last_beat_t;
                while gap > 0.9 {
                    gap *= 0.5;
                }
                if gap >= 0.3 {
                    live.ibis.push_back(gap);
                    if live.ibis.len() > 15 {
                        live.ibis.pop_front();
                    }
                    let mut v: Vec<f32> = live.ibis.iter().copied().collect();
                    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    live.period = v[v.len() / 2];
                    live.gen.beat_len = live.period;
                }
                live.last_beat_t = o.t;
                live.beat_n += 1;
                fresh.push(LevelEvent { t: o.t, lead: 0.0, ev: Ev::Beat { down: live.beat_n % 4 == 1 } });
            }
        }
        live.pending.extend(fresh);

        let clock = (self.audio.live_time() as f32).max(live.last_clock);
        live.last_clock = clock;
        while live.feats.len() > 2 && live.feats[1].0 < clock - 2.0 {
            live.feats.pop_front();
        }

        // Is there sound around the audible moment?
        let mut heard = false;
        let mut feat = Feat::default();
        let mut best = f32::MAX;
        for &(ft, f, raw) in live.feats.iter() {
            if ft > clock - 1.2 && ft < clock + 0.6 && raw > 0.003 {
                heard = true;
            }
            let d = (ft - clock).abs();
            if d < best {
                best = d;
                feat = f;
            }
        }
        if best > 0.25 {
            feat = Feat::default();
        }
        let stalled = !info.delayed && info.since_packet > 0.35;
        let signal = heard && !info.rebuffering && !stalled;
        if signal {
            live.silent_for = 0.0;
        } else {
            live.silent_for += dt;
        }
        let idle_now = live.silent_for > 1.2;
        if idle_now && !live.was_idle {
            self.world.calm_down();
            live.pending.clear();
        }
        if !idle_now && live.was_idle {
            live.gen.resume();
            if self.world.heart.broken {
                live.hearts += 1;
                self.world.begin_song(210.0, live.hearts, 0xC0FFEE + live.hearts as u64);
            }
        }
        live.was_idle = idle_now;

        // Fire due events.
        self.world.t = clock;
        if !live.pending.is_empty() {
            let mut i = 0;
            while i < live.pending.len() {
                if live.pending[i].at() <= clock {
                    let e = live.pending.swap_remove(i);
                    // Drop anything hopelessly stale (e.g. after a stall).
                    if clock - e.t < 1.0 {
                        self.world.apply(&e);
                    }
                } else {
                    i += 1;
                }
            }
        }
        let intensity = live.gen.intensity;
        // Prefer the analyser's tempo tracker once it is confident; until then follow the kicks.
        let live_bp = if live_beat.conf > 0.4 && live_beat.period > 0.2 {
            live.period = live_beat.period;
            live.gen.beat_len = live.period;
            (clock - live_beat.phase_t) / live_beat.period
        } else {
            live.beat_n as f32 + ((clock - live.last_beat_t) / live.period.max(0.2)).clamp(0.0, 0.999)
        };
        self.world.beat_len = live.period;
        self.world.update(dt, clock, input, live_bp, &feat, intensity, !idle_now);

        hud.mode = if info.delayed {
            format!("LIVE  -  DELAYED {:.1}s LOOK-AHEAD", live.delay)
        } else {
            "LIVE  -  REACTIVE".into()
        };
        if info.rebuffering {
            hud.status = format!("BUFFERING  {:.1} / {:.1}s", info.buffered.clamp(0.0, info.delay), info.delay);
        } else if idle_now {
            hud.status = "WAITING FOR AUDIO".into();
        } else if !status.is_empty() {
            hud.status = status;
        }
        if self.world.dead {
            self.phase = Phase::Dying(0.0);
        }
        (if idle_now { 1.0 } else { 0.0 }, hud, 0.0)
    }
}

fn hud_progress(t: f32, dur: f32) -> f32 {
    (t / dur.max(0.1)).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------------------------
// Simple immediate-mode UI
// ---------------------------------------------------------------------------------------------

/// Draws a vertical list of buttons. Returns the index that was activated (click / Enter).
fn menu_list(items: &[String], sel: &mut usize, x: f32, y: f32, u: f32, rend: &Renderer, audio: &Audio) -> Option<usize> {
    let before = *sel;
    let (mx, my) = mouse_position();
    let h = 46.0 * u;
    let w = 520.0 * u;
    let mut activated = None;
    if is_key_pressed(KeyCode::Down) {
        *sel = (*sel + 1) % items.len();
    }
    if is_key_pressed(KeyCode::Up) {
        *sel = (*sel + items.len() - 1) % items.len();
    }
    for (i, it) in items.iter().enumerate() {
        let by = y + i as f32 * (h + 8.0 * u);
        let hover = mx >= x && mx <= x + w && my >= by && my <= by + h;
        if hover && mouse_delta_position() != Vec2::ZERO {
            *sel = i;
        }
        rend.draw_button(x, by, w, h, it, *sel == i, u);
        if hover && is_mouse_button_pressed(MouseButton::Left) {
            *sel = i;
            activated = Some(i);
        }
    }
    if is_key_pressed(KeyCode::Enter) || is_key_pressed(KeyCode::KpEnter) {
        activated = Some(*sel);
    }
    if activated.is_some() {
        audio.sfx(Sfx::UiSelect, 0.8, 0.0);
    } else if *sel != before {
        audio.sfx(Sfx::UiMove, 0.6, 0.0);
    }
    activated
}

fn dim(a: f32) {
    draw_rectangle(0.0, 0.0, screen_width(), screen_height(), Color::new(0.02, 0.0, 0.06, a));
}


// ---------------------------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------------------------

async fn run(args: Args) {
    let audio = Audio::new();
    let rend = Renderer::new(args.ss);
    let diff = args.diff;
    let mut app = App {
        audio,
        rend,
        world: World::new(DIFFS[diff].0, diff == 0),
        screen: Screen::Menu,
        phase: Phase::Loading,
        queue: Vec::new(),
        qi: 0,
        loader: None,
        ready: None,
        song: None,
        live: None,
        idle: 1.0,
        diff,
        volume: 0.8,
        sfx_volume: 0.5,
        offset_ms: 0.0,
        music_dir: None,
        juke: Jukebox { list: Vec::new(), idx: 0, loaded: None, loader: None, cursor: 0, last_t: -1.0, playing: false, fails: 0 },
        autofire: true,
        fullscreen: false,
        menu_sel: 0,
        dialog: None,
        dialog_library: false,
        esc_used: false,
        message: String::new(),
        message_t: 0.0,
        died: false,
        cap_sel: 0,
        out_sel: 0,
        delay: 3.0,
        shots_done: 0,
        frames: 0,
        wall: 0.0,
        max_bullets: 0,
        max_enemies: 0,
        log: Vec::new(),
        args: args.clone(),
    };
    // ---- settings ----
    let cfg = load_settings();
    let getf = |k: &str, d: f32| -> f32 { cfg.get(k).and_then(|v| v.parse().ok()).unwrap_or(d) };
    if !args.diff_set {
        app.diff = (getf("diff", 1.0) as usize).min(2);
    }
    app.volume = getf("volume", 0.8).clamp(0.0, 1.0);
    app.sfx_volume = getf("sfx", 0.5).clamp(0.0, 1.0);
    app.offset_ms = getf("offset_ms", 0.0).clamp(-100.0, 300.0);
    app.autofire = getf("autofire", 1.0) > 0.5;
    app.music_dir = cfg.get("music_dir").map(PathBuf::from).filter(|p| p.is_dir()).or_else(|| music_folder().filter(|p| p.is_dir()));
    app.world = World::new(app.diff(), app.diff == 0);
    app.juke_rescan();
    app.audio.set_volume(if args.mute { 0.0 } else { app.volume });
    app.audio.set_sfx_volume(if args.mute { 0.0 } else { app.sfx_volume });
    app.audio.ensure_output();
    if let Some(dir) = &args.selftest {
        let _ = std::fs::create_dir_all(dir);
    }
    if args.screen == "live" {
        app.audio.refresh_devices();
        app.screen = Screen::LiveSetup;
    } else if args.live {
        // Devices are enumerated on the audio thread; give it a moment.
        for _ in 0..60 {
            if !app.audio.devices().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let devs = app.audio.devices();
        let find = |needle: &str| devs.iter().position(|d| d.name.to_lowercase().contains(&needle.to_lowercase()));
        if !args.live_in.is_empty() {
            if let Some(i) = find(&args.live_in) {
                app.cap_sel = i;
            }
        }
        if !args.live_out.is_empty() {
            if let Some(i) = find(&args.live_out) {
                app.out_sel = i + 1;
            }
        }
        app.start_live();
    } else if !args.files.is_empty() {
        app.start_run(args.files.clone());
    }

    loop {
        let dt = get_frame_time().clamp(0.0005, 0.05);
        app.frames += 1;
        app.wall += dt;
        app.message_t = (app.message_t - dt).max(0.0);
        app.esc_used = false;

        // ---- global keys ----
        if is_key_pressed(KeyCode::F11) {
            app.fullscreen = !app.fullscreen;
            set_fullscreen(app.fullscreen);
        }
        if is_key_pressed(KeyCode::F) && app.screen == Screen::Play {
            app.autofire = !app.autofire;
            let p = app.world.player.pos + vec2(0.0, -40.0);
            app.world.text(p, if app.autofire { "AUTO-FIRE ON" } else { "AUTO-FIRE OFF (hold LMB)" }, game::C_WHITE, 20.0, 1.2);
        }

        // ---- dropped files / dialog results ----
        let dropped: Vec<PathBuf> = get_dropped_files().into_iter().filter_map(|f| f.path).collect();
        if !dropped.is_empty() && matches!(app.screen, Screen::Menu | Screen::Results | Screen::LiveSetup) {
            let paths = expand_paths(&dropped);
            app.start_run(paths);
        }
        let mut picked: Option<Vec<PathBuf>> = None;
        if let Some(rx) = &app.dialog {
            match rx.try_recv() {
                Ok(p) => picked = Some(p),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => picked = Some(vec![]),
                Err(_) => {}
            }
        }
        if let Some(p) = picked {
            app.dialog = None;
            if app.dialog_library {
                app.dialog_library = false;
                if let Some(dir) = p.into_iter().next() {
                    app.music_dir = Some(dir);
                    app.juke_rescan();
                    app.save_settings();
                }
            } else if !p.is_empty() {
                let paths = expand_paths(&p);
                app.start_run(paths);
            }
        }

        app.poll_loader();
        app.rend.update_view(&app.world);

        // ---- simulate ----
        let bot = app.args.selftest.is_some();
        let mut hud = HudInfo::empty();
        let mut idle_target = 1.0;
        let mut progress = 0.0;
        match app.screen {
            Screen::Play => {
                let mut input = if bot { tools::bot_input(&app.world) } else { app.gather_input(true) };
                if bot && app.args.mortal {
                    input.mv = Vec2::ZERO;
                    input.dash = false;
                }
                let (it, h, pr) = if app.live.is_some() { app.step_live(dt, &input) } else { app.step_files(dt, &input) };
                idle_target = it;
                hud = h;
                progress = pr;
                let esc = is_key_pressed(KeyCode::Escape);
                let pause_key = esc || is_key_pressed(KeyCode::P);
                if pause_key && app.screen == Screen::Play {
                    app.esc_used = true;
                    if app.live.is_some() {
                        if esc {
                            app.to_menu();
                        }
                    } else {
                        app.audio.set_paused(true);
                        app.screen = Screen::Paused;
                        app.menu_sel = 0;
                        app.audio.sfx(Sfx::UiSelect, 0.7, 0.0);
                    }
                }
            }
            Screen::Paused => {
                idle_target = app.idle;
                if let Some(s) = &app.song {
                    hud.title = s.track.name.clone();
                    hud.progress = s.last_t / s.level.duration.max(0.1);
                    hud.show_combat = true;
                    progress = hud.progress;
                }
            }
            _ => {
                // Menu-like screens: drift mode with the menu music; the player can still fly around.
                let mut input = app.gather_input(false);
                input.dash = false;
                progress = app.juke_step(dt, &input);
            }
        }
        // Sound effects the simulation asked for (not in live mode: the capture would hear them).
        let quiet = app.live.is_some();
        for (s, gain, pan) in app.world.sounds.drain(..) {
            if !quiet {
                app.audio.sfx(s, gain, pan);
            }
        }

        // ---- idle blend ----
        let rate = if idle_target > app.idle { 1.3 } else { 1.0 };
        let prev_idle = app.idle;
        app.idle += (idle_target - app.idle).clamp(-rate * dt, rate * dt);
        if prev_idle <= 0.5 && app.idle > 0.5 && app.screen == Screen::Play {
            app.world.calm_down();
        }

        // ---- draw ----
        app.rend.set_palette(app.world.palette);
        app.rend.draw_scene(&app.world, dt, app.idle, progress);
        app.rend.draw_hud(&app.world, &hud, app.idle);
        let pal = app.rend.pal;
        let (sw, sh) = (screen_width(), screen_height());
        let u = sh / 1080.0;

        match app.screen {
            Screen::Menu => {
                app.rend.draw_logo(110.0 * u, 96.0 * u, u, &app.world);
                let lib_name = app
                    .music_dir
                    .as_ref()
                    .and_then(|d| d.file_name())
                    .and_then(|s| s.to_str())
                    .unwrap_or("none")
                    .to_string();
                let n_lib = app.juke.list.len().saturating_sub(synth::THEME_COUNT);
                let items = vec![
                    "PLAY THIS TRACK".to_string(),
                    "SHUFFLE RUN   (5 tracks)".to_string(),
                    "PLAY FILES...".to_string(),
                    "PLAY FOLDER...".to_string(),
                    format!("MUSIC LIBRARY   {lib_name}  ({n_lib} tracks)"),
                    "LIVE AUDIO   (what your PC is playing)".to_string(),
                    format!("DIFFICULTY   {}", DIFFS[app.diff].1),
                    format!("MUSIC VOLUME   {:.0}%", app.volume * 100.0),
                    format!("EFFECTS VOLUME   {:.0}%", app.sfx_volume * 100.0),
                    format!("SYNC OFFSET   {:+.0} ms", app.offset_ms),
                    format!("FULLSCREEN   {}", if app.fullscreen { "ON" } else { "OFF" }),
                    "QUIT".to_string(),
                ];
                let act = if app.dialog.is_none() { menu_list(&items, &mut app.menu_sel, 110.0 * u, 280.0 * u, u, &app.rend, &app.audio) } else { None };
                // left/right adjusts the selected row; on the play rows it changes the track
                let dirk = is_key_pressed(KeyCode::Right) as i32 - is_key_pressed(KeyCode::Left) as i32;
                let track_dirk = is_key_pressed(KeyCode::E) as i32 - is_key_pressed(KeyCode::Q) as i32 + if app.menu_sel <= 1 { dirk } else { 0 };
                if track_dirk != 0 && app.dialog.is_none() {
                    app.juke_next(track_dirk);
                }
                let mut changed = false;
                if dirk != 0 {
                    changed = true;
                    match app.menu_sel {
                        6 => app.diff = ((app.diff as i32 + dirk).rem_euclid(3)) as usize,
                        7 => app.volume = (app.volume + 0.1 * dirk as f32).clamp(0.0, 1.0),
                        8 => app.sfx_volume = (app.sfx_volume + 0.1 * dirk as f32).clamp(0.0, 1.0),
                        9 => app.offset_ms = (app.offset_ms + 10.0 * dirk as f32).clamp(-100.0, 300.0),
                        _ => changed = false,
                    }
                }
                match act {
                    Some(0) => app.start_from_juke(0),
                    Some(1) => app.start_from_juke(4),
                    Some(2) => {
                        let (tx, rx) = channel();
                        std::thread::spawn(move || {
                            let exts: Vec<&str> = decode::AUDIO_EXTS.to_vec();
                            let r = rfd::FileDialog::new().set_title("Heartline - choose songs").add_filter("Audio", &exts).pick_files();
                            let _ = tx.send(r.unwrap_or_default());
                        });
                        app.dialog = Some(rx);
                        app.dialog_library = false;
                    }
                    Some(3) | Some(4) => {
                        let library = act == Some(4);
                        let (tx, rx) = channel();
                        let start_dir = app.music_dir.clone();
                        std::thread::spawn(move || {
                            let mut d = rfd::FileDialog::new().set_title(if library { "Heartline - choose your music library folder" } else { "Heartline - choose a music folder to play" });
                            if let Some(sd) = start_dir {
                                d = d.set_directory(sd);
                            }
                            let _ = tx.send(d.pick_folder().map(|p| vec![p]).unwrap_or_default());
                        });
                        app.dialog = Some(rx);
                        app.dialog_library = library;
                    }
                    Some(5) => {
                        app.audio.refresh_devices();
                        app.screen = Screen::LiveSetup;
                        app.menu_sel = 0;
                    }
                    Some(6) => {
                        app.diff = (app.diff + 1) % 3;
                        changed = true;
                    }
                    Some(7) => {
                        app.volume = if app.volume >= 0.99 { 0.0 } else { (app.volume + 0.1).min(1.0) };
                        changed = true;
                    }
                    Some(8) => {
                        app.sfx_volume = if app.sfx_volume >= 0.99 { 0.0 } else { (app.sfx_volume + 0.1).min(1.0) };
                        changed = true;
                    }
                    Some(9) => {
                        app.offset_ms = if app.offset_ms >= 295.0 { -100.0 } else { app.offset_ms + 10.0 };
                        changed = true;
                    }
                    Some(10) => {
                        app.fullscreen = !app.fullscreen;
                        set_fullscreen(app.fullscreen);
                    }
                    Some(11) => break,
                    _ => {}
                }
                if changed {
                    let mute = app.args.mute;
                    app.audio.set_volume(if mute { 0.0 } else { app.volume });
                    app.audio.set_sfx_volume(if mute { 0.0 } else { app.sfx_volume });
                    if app.menu_sel == 8 {
                        app.audio.sfx(Sfx::Shard, 1.0, 0.0);
                    }
                    app.save_settings();
                }
                // Now-playing card
                let (title, hint) = match (&app.juke.loaded, app.juke.list.get(app.juke.idx)) {
                    (_, None) => ("no music found".to_string(), "choose MUSIC LIBRARY or drop files on the window".to_string()),
                    (Some((p, l)), Some(want)) if p == want => (l.track.name.clone(), "Q / E  change track        ENTER  play it".to_string()),
                    (_, Some(want)) => (track_title(want), "reading the music...".to_string()),
                };
                let title: String = if title.chars().count() > 38 { title.chars().take(37).chain("…".chars()).collect() } else { title };
                app.rend.draw_now_playing(sw - 700.0 * u, 110.0 * u, 590.0 * u, u, &app.world, &title, &hint, progress);
                let hy = sh - 60.0 * u;
                let help = match app.menu_sel {
                    9 => "SYNC OFFSET: raise it if attacks land before you hear the beat (soundbar / Bluetooth lag), lower it if they land late",
                    4 => "MUSIC LIBRARY: the folder the menu plays from and SHUFFLE RUN draws its tracks from",
                    1 => "SHUFFLE RUN: this track plus four more; HP, upgrades and score carry over and the Heart gets tougher",
                    _ => "drop audio files or folders on the window to play them",
                };
                text_shadow(help, 110.0 * u, hy, 22.0 * u, Color::new(1.0, 1.0, 1.0, 0.8), -1);
                text_shadow("WASD move   mouse aim   SPACE dash   F auto-fire   ESC / P pause   F11 fullscreen", 110.0 * u, hy + 28.0 * u, 20.0 * u, Color::new(1.0, 1.0, 1.0, 0.55), -1);
                if app.dialog.is_some() {
                    text_shadow("waiting for the file dialog...", sw * 0.5, sh * 0.5, 28.0 * u, pal.accent, 0);
                }
            }
            Screen::LiveSetup => {
                dim(0.66);
                let devs = app.audio.devices();
                let x = 110.0 * u;
                render::vtext("LIVE AUDIO", x, 130.0 * u, 64.0 * u, WHITE, -1);
                text_left("Heartline listens to an output device and builds the level on the fly.", x, 170.0 * u, 24.0 * u, pal.primary);

                let (mx, my) = mouse_position();
                let click = is_mouse_button_pressed(MouseButton::Left);
                let row_h = 34.0 * u;
                let col_w = 800.0 * u;
                // capture column
                let mut y = 230.0 * u;
                text_left("LISTEN TO", x, y, 26.0 * u, pal.accent);
                y += 14.0 * u;
                for (i, d) in devs.iter().enumerate().take(9) {
                    let hover = mx >= x && mx <= x + col_w && my >= y && my <= y + row_h;
                    let on = app.cap_sel == i;
                    draw_rectangle(x, y, col_w, row_h - 4.0 * u, if on { Color::new(pal.secondary.r, pal.secondary.g, pal.secondary.b, 0.3) } else { Color::new(0.0, 0.0, 0.05, 0.45) });
                    let label = format!("{}{}", d.name, if d.is_default { "   (default)" } else { "" });
                    text_left(&label, x + 14.0 * u, y + row_h * 0.62, 22.0 * u, if on { WHITE } else { Color::new(1.0, 1.0, 1.0, 0.7) });
                    if hover && click {
                        app.cap_sel = i;
                    }
                    y += row_h;
                }
                y += 34.0 * u;
                text_left("PLAY IT BACK ON  (delayed look-ahead mode)", x, y, 26.0 * u, pal.accent);
                y += 14.0 * u;
                let mut outs: Vec<String> = vec!["nowhere - reactive mode, zero setup".to_string()];
                outs.extend(devs.iter().map(|d| d.name.clone()));
                for (i, name) in outs.iter().enumerate().take(10) {
                    let disabled = i > 0 && i - 1 == app.cap_sel;
                    let hover = mx >= x && mx <= x + col_w && my >= y && my <= y + row_h;
                    let on = app.out_sel == i;
                    draw_rectangle(x, y, col_w, row_h - 4.0 * u, if on { Color::new(pal.secondary.r, pal.secondary.g, pal.secondary.b, 0.3) } else { Color::new(0.0, 0.0, 0.05, 0.45) });
                    let col = if disabled { Color::new(1.0, 1.0, 1.0, 0.25) } else if on { WHITE } else { Color::new(1.0, 1.0, 1.0, 0.7) };
                    text_left(name, x + 14.0 * u, y + row_h * 0.62, 22.0 * u, col);
                    if hover && click && !disabled {
                        app.out_sel = i;
                    }
                    y += row_h;
                }
                if app.out_sel > 0 && app.out_sel - 1 == app.cap_sel {
                    app.out_sel = 0;
                }
                // explanation
                let ex = x + col_w + 60.0 * u;
                let mut ey = 244.0 * u;
                let lines: [&str; 12] = [
                    "REACTIVE",
                    "Pick the device your music plays on and",
                    "leave playback on 'nowhere'. The Heart",
                    "reacts the instant a hit is heard.",
                    "",
                    "DELAYED LOOK-AHEAD",
                    "Send your music app to a second or virtual",
                    "output (e.g. a virtual cable or unused HDMI),",
                    "listen to that here, and play it back on your",
                    "real speakers. You hear it a few seconds late",
                    "and every spawn is telegraphed to land on",
                    "the beat, like a fully analysed song.",
                ];
                for l in lines {
                    let head = l == "REACTIVE" || l == "DELAYED LOOK-AHEAD";
                    text_left(l, ex, ey, if head { 24.0 * u } else { 21.0 * u }, if head { pal.accent } else { Color::new(1.0, 1.0, 1.0, 0.8) });
                    ey += 28.0 * u;
                }
                ey += 20.0 * u;
                text_left(&format!("DELAY   {:.1}s     ( - / + )", app.delay), ex, ey, 24.0 * u, pal.primary);
                if is_key_pressed(KeyCode::Minus) || is_key_pressed(KeyCode::KpSubtract) {
                    app.delay = (app.delay - 0.5).max(1.5);
                }
                if is_key_pressed(KeyCode::Equal) || is_key_pressed(KeyCode::KpAdd) {
                    app.delay = (app.delay + 0.5).min(6.0);
                }
                // buttons
                let by = sh - 130.0 * u;
                let start_hover = mx >= x && mx <= x + 300.0 * u && my >= by && my <= by + 54.0 * u;
                draw_rectangle(x, by, 300.0 * u, 54.0 * u, Color::new(pal.secondary.r, pal.secondary.g, pal.secondary.b, if start_hover { 0.5 } else { 0.3 }));
                draw_rectangle_lines(x, by, 300.0 * u, 54.0 * u, 2.0 * u, pal.accent);
                text_center("START   [ENTER]", x + 150.0 * u, by + 36.0 * u, 28.0 * u, WHITE);
                text_left("ESC  back", x + 330.0 * u, by + 36.0 * u, 24.0 * u, Color::new(1.0, 1.0, 1.0, 0.7));
                if devs.is_empty() {
                    text_left("no output devices found", x, by - 20.0 * u, 24.0 * u, pal.danger);
                } else if (start_hover && click) || is_key_pressed(KeyCode::Enter) {
                    app.start_live();
                }
                if is_key_pressed(KeyCode::Escape) {
                    app.screen = Screen::Menu;
                    app.menu_sel = 2;
                }
            }
            Screen::Paused => {
                dim(0.55);
                render::vtext("PAUSED", sw * 0.5, sh * 0.3, 80.0 * u, WHITE, 0);
                let items = vec!["RESUME".to_string(), "RESTART RUN".to_string(), "QUIT TO MENU".to_string()];
                let act = menu_list(&items, &mut app.menu_sel, sw * 0.5 - 260.0 * u, sh * 0.42, u, &app.rend, &app.audio);
                if ((is_key_pressed(KeyCode::Escape) || is_key_pressed(KeyCode::P)) && !app.esc_used) || act == Some(0) {
                    app.audio.set_paused(false);
                    app.screen = Screen::Play;
                } else if act == Some(1) {
                    let q = app.queue.clone();
                    app.start_run(q);
                } else if act == Some(2) {
                    app.to_menu();
                }
            }
            Screen::Results => {
                app.world.orbs.clear();
                dim(0.64);
                let st = app.world.stats;
                let head = if app.died { "SIGNAL LOST" } else { "RUN COMPLETE" };
                render::vtext(head, sw * 0.5, sh * 0.2, 84.0 * u, if app.died { pal.danger } else { pal.accent }, 0);
                text_center(&format!("{:08}", app.world.score), sw * 0.5, sh * 0.32, 96.0 * u, WHITE);
                let rank = if app.died {
                    "C"
                } else if st.hits == 0 && st.hearts_broken >= st.songs.max(1) {
                    "S"
                } else if st.hearts_broken >= st.songs.max(1) {
                    "A"
                } else {
                    "B"
                };
                text_center(&format!("RANK  {rank}"), sw * 0.5, sh * 0.39, 44.0 * u, pal.primary);
                let rows = [
                    ("tracks cleared", format!("{}", st.songs)),
                    ("hearts broken", format!("{}", st.hearts_broken)),
                    ("enemies destroyed", format!("{}", st.kills)),
                    ("notes shot down", format!("{}", st.notes_shot)),
                    ("shards collected", format!("{}", st.shards)),
                    ("best combo", format!("{:.0}", st.max_combo)),
                    ("hits taken", format!("{}", st.hits)),
                ];
                let mut y = sh * 0.47;
                for (k, v) in rows {
                    text_left(k, sw * 0.5 - 230.0 * u, y, 28.0 * u, Color::new(1.0, 1.0, 1.0, 0.75));
                    let w = text_w(&v, 28.0 * u);
                    text_left(&v, sw * 0.5 + 230.0 * u - w, y, 28.0 * u, WHITE);
                    y += 36.0 * u;
                }
                let can_retry = !app.queue.is_empty();
                let hint = if can_retry { "ENTER  play again        ESC  menu" } else { "ESC  menu" };
                text_center(hint, sw * 0.5, sh * 0.82, 28.0 * u, pal.accent);
                if is_key_pressed(KeyCode::Escape) {
                    app.to_menu();
                } else if can_retry && is_key_pressed(KeyCode::Enter) {
                    let q = app.queue.clone();
                    app.start_run(q);
                }
            }
            Screen::Play => {}
        }

        if app.message_t > 0.0 && !app.message.is_empty() {
            let a = app.message_t.min(1.0);
            text_shadow(&app.message, sw * 0.5, sh * 0.92, 24.0 * u, Color::new(pal.danger.r, pal.danger.g, pal.danger.b, a), 0);
        }

        // ---- self-test hooks ----
        if let Some(dir) = app.args.selftest.clone() {
            app.max_bullets = app.max_bullets.max(app.world.bullets.len());
            app.max_enemies = app.max_enemies.max(app.world.enemies.len());
            let st = app.song.as_ref().map(|s| s.last_t).unwrap_or(-1.0);
            let key = if app.song.is_some() || app.live.is_some() { if app.live.is_some() { app.wall } else { st } } else { -1.0 };
            let next = app.args.shots.get(app.shots_done).copied();
            let menu_shot = app.args.wall || (app.args.files.is_empty() && !app.args.live);
            let due = match next {
                Some(tn) => (menu_shot && app.wall >= tn) || (!menu_shot && key >= tn),
                None => false,
            };
            if due {
                let tn = next.unwrap();
                let path = dir.join(format!("shot_{:03}_{:06.2}.png", app.shots_done, tn));
                get_screen_data().export_png(&path.to_string_lossy());
                app.log.push(format!(
                    "shot {} at t={:.2} wall={:.2} idle={:.2} bullets={} enemies={} score={} hp={} fps={:.0}",
                    app.shots_done,
                    key,
                    app.wall,
                    app.idle,
                    app.world.bullets.len(),
                    app.world.enemies.len(),
                    app.world.score,
                    app.world.player.hp,
                    get_fps()
                ));
                app.shots_done += 1;
            }
            let over = app.wall >= app.args.secs || app.screen == Screen::Results && app.wall > 3.0 && app.shots_done >= app.args.shots.len();
            if over {
                app.log.push(format!(
                    "done wall={:.1}s frames={} avg_fps={:.1} max_bullets={} max_enemies={} score={} kills={} hits={} screen={} status='{}'",
                    app.wall,
                    app.frames,
                    app.frames as f32 / app.wall.max(0.01),
                    app.max_bullets,
                    app.max_enemies,
                    app.world.score,
                    app.world.stats.kills,
                    app.world.stats.hits,
                    app.screen as i32,
                    app.audio.status()
                ));
                let _ = std::fs::write(dir.join("selftest.log"), app.log.join("\n"));
                break;
            }
        }

        next_frame().await;
    }
    app.save_settings();
    app.audio.stop();
    app.audio.stop_live();
}
