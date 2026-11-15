//! Heartline — your music becomes a top-down vaporwave bullet-hell.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analysis;
mod decode;
mod display;
mod engine;
mod game;
mod level;
mod net;
mod rally;
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
use rally::{Character, FxKind, Rally, RallyInput, RallyMusic};
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
    /// LAN test: host a Rally match with this song.
    lan_host: Option<PathBuf>,
    /// LAN test: join the host at this IP (optionally `ip:port`).
    lan_join: Option<String>,
    /// LAN test: the local Rally player is the bot (auto character, auto ready).
    rally_bot: bool,
    /// Offline Rally practice with this song: you (or `--rally-bot`) vs the bot, no network.
    rally_local: Option<PathBuf>,
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
            "--lan-host" => a.lan_host = Some(PathBuf::from(val())),
            "--lan-join" => a.lan_join = Some(val()),
            "--rally-bot" => a.rally_bot = true,
            "--rally-local" => a.rally_local = Some(PathBuf::from(val())),
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
    /// LAN Rally: host/join, lobby, countdown, match and results (`App::lan` holds the stage).
    Lan,
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

/// Menu music plays at this fraction of the in-run music volume.
const MENU_MUSIC_GAIN: f32 = 0.5;

const DIFFS: [(f32, &str); 3] = [(0.65, "CHILL  (no-fail)"), (1.0, "NORMAL"), (1.35, "HARD")];
/// Menu VISUALS setting: scale of the renderer's music-reactive pulse layer.
const VISUALS: [(f32, &str); 3] = [(1.0, "FULL"), (0.45, "SUBTLE"), (0.0, "OFF")];

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
    /// Smoothed music gain (menu duck, death fade).
    music_gain: f32,
    /// Extra audio latency in ms (soundbars, Bluetooth): events are delayed by this much.
    offset_ms: f32,
    music_dir: Option<PathBuf>,
    juke: Jukebox,
    autofire: bool,
    fullscreen: bool,
    /// Index into `VISUALS`.
    visuals: usize,
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
    /// LAN Rally session (Some while `screen == Screen::Lan`).
    lan: Option<Box<Lan>>,
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

/// Seconds from `t` until the next section that steps UP into Intense / Drop (infinite if none).
/// The renderer builds tension over the bars before it and releases it on the downbeat.
fn next_build(an: &Analysis, t: f32) -> f32 {
    let big = |k: SectionKind| matches!(k, SectionKind::Intense | SectionKind::Drop);
    let mut prev_big = false;
    for s in &an.sections {
        if s.t0 > t + 0.02 && big(s.kind) && !prev_big {
            return s.t0 - t;
        }
        prev_big = big(s.kind);
    }
    f32::INFINITY
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

    /// Hand the renderer this frame's audible waveform window and the time to the next big
    /// section (file / menu playback only; live mode gets neither).
    fn feed_signal(&mut self) {
        self.rend.fx = VISUALS[self.visuals].0;
        let t = self.world.t;
        let src = if self.live.is_some() {
            None
        } else if matches!(self.screen, Screen::Play | Screen::Paused) {
            match (self.phase, self.song.as_ref()) {
                (Phase::Song, Some(s)) => Some((&s.track, &s.an)),
                _ => None,
            }
        } else if self.juke.playing {
            self.juke.loaded.as_ref().map(|(_, l)| (&l.track, &l.an))
        } else {
            None
        };
        match src {
            Some((tr, an)) => self.rend.feed_signal(Some((&tr.samples, tr.sr)), t, next_build(an, t)),
            None => self.rend.feed_signal(None, t, f32::INFINITY),
        }
    }

    fn save_settings(&self) {
        if self.args.selftest.is_some() {
            return;
        }
        let Some(path) = settings_path() else { return };
        let mut text = format!(
            "diff={}\nvolume={:.2}\nsfx={:.2}\noffset_ms={:.0}\nautofire={}\nfullscreen={}\n",
            self.diff, self.volume, self.sfx_volume, self.offset_ms, self.autofire as u8, self.fullscreen as u8
        );
        text.push_str(&format!("visuals={}\n", self.visuals));
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
// LAN Rally: lobby and match flow (sockets, protocol and threads live in net.rs)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LanStage {
    Choose,
    Join,
    Lobby,
    Countdown,
    Match,
    Results,
}

/// Inputs (client) and snapshots (host) go out at this fixed rate, whatever the frame rate.
const LAN_SEND_DT: f32 = 1.0 / 60.0;
const LAN_COUNTDOWN: f64 = 3.0;
const LAN_HIST: usize = 128;
const CHAR_DESC: [&str; 4] = [
    "bass: perfect returns slam the other player back",
    "snare: perfect returns split the orb in two",
    "hat: perfect returns zig-zag every half beat",
    "lead: perfect returns curve and leave a laser trail",
];

struct LanMatch {
    track: Arc<Track>,
    an: Arc<Analysis>,
    events: Vec<LevelEvent>,
    beats: Vec<f32>,
    bars: Vec<f32>,
    duration: f32,
    cursor: usize,
    /// Song time of this frame (negative during the countdown).
    t: f32,
    /// Local `net::now()` at which both PCs start the song.
    start_local: f64,
    audio_on: bool,
    /// Host: the authoritative state. Client: the latest snapshot.
    rally: Rally,
    /// Client: the snapshot before `rally`, and the interpolated + predicted state that is drawn.
    prev: Rally,
    view: Rally,
    prev_at: f64,
    cur_at: f64,
    snaps: u32,
    bad_snaps: u32,
    snap_t: f32,
    buf: Vec<u8>,
    send_acc: f32,
    // host: the client's latest input
    peer_in: RallyInput,
    peer_swings: u8,
    peer_pulse: bool,
    peer_seq: u32,
    // client: own inputs, kept until the host acknowledges them
    seq: u32,
    swings: u8,
    down: bool,
    ack: u32,
    hist: VecDeque<(u32, RallyInput, f32)>,
    fx_seen: f32,
    score_seen: [u32; 2],
    confirm: bool,
    traffic0: (u64, u64),
    wall0: f64,
}

struct Lan {
    host: bool,
    stage: LanStage,
    net: Option<net::Net>,
    disc: Option<net::Discovery>,
    sel: usize,
    ip: String,
    my_ip: String,
    status: String,
    peer: String,
    connected: bool,
    my_char: u8,
    peer_char: u8,
    my_ready: bool,
    peer_ready: bool,
    /// Host: the chosen song. Client: the received file (or built-in theme path).
    song: Option<PathBuf>,
    song_title: String,
    /// Host: the song last sent to the client, and whether it has fully gone out.
    sent: Option<PathBuf>,
    song_sent: bool,
    xfer: (u64, u64),
    loader: Option<(PathBuf, Loader)>,
    loaded: Option<(PathBuf, Loaded)>,
    peer_loaded: bool,
    told_loaded: bool,
    /// (offset = host clock - local clock, best RTT, samples)
    clock: (f64, f64, u32),
    dialog: Option<Receiver<Vec<PathBuf>>>,
    m: Option<Box<LanMatch>>,
    result: [u32; 2],
    result_t: f32,
    log_path: Option<PathBuf>,
    log: Vec<String>,
    /// Offline practice (`--rally-local`): no network, the peer is the bot.
    local: bool,
}

impl Lan {
    fn new(log_dir: Option<&Path>) -> Lan {
        Lan {
            local: false,
            host: false,
            stage: LanStage::Choose,
            net: None,
            disc: None,
            sel: 0,
            ip: String::new(),
            my_ip: String::new(),
            status: String::new(),
            peer: String::new(),
            connected: false,
            my_char: 0,
            peer_char: 0,
            my_ready: false,
            peer_ready: false,
            song: None,
            song_title: String::new(),
            sent: None,
            song_sent: false,
            xfer: (0, 0),
            loader: None,
            loaded: None,
            peer_loaded: false,
            told_loaded: false,
            clock: (0.0, 0.0, 0),
            dialog: None,
            m: None,
            result: [0; 2],
            result_t: 0.0,
            log_path: log_dir.map(|d| d.join("lan.log")),
            log: Vec::new(),
        }
    }

    /// Record a state change (selftest runs write them to `lan.log`).
    fn note(&mut self, s: String) {
        if let Some(p) = &self.log_path {
            self.log.push(format!("{:9.3} {} {}", net::now(), if self.host { "host" } else { "client" }, s));
            let _ = std::fs::write(p, self.log.join("\n") + "\n");
        }
    }

    fn send(&self, m: &net::Msg) {
        if let Some(n) = &self.net {
            n.send(m);
        }
    }

    fn send_lobby(&self) {
        self.send(&net::Msg::Lobby { ch: self.my_char, ready: self.my_ready });
    }

    fn in_match(&self) -> bool {
        matches!(self.stage, LanStage::Countdown | LanStage::Match) && self.m.is_some()
    }

    fn set_song(&mut self, p: Option<PathBuf>) {
        self.song_title = p.as_deref().map(track_title).unwrap_or_default();
        self.song = p;
    }

    fn is_loaded(&self) -> bool {
        matches!((&self.loaded, &self.song), (Some((a, _)), Some(b)) if a == b)
    }
}

/// 0..1 build-up ramp before the next Intense / Drop section (same window as the renderer).
fn build_ramp(an: &Analysis, t: f32) -> f32 {
    let beat = if an.bpm > 1.0 { 60.0 / an.bpm } else { 0.5 };
    let win = (16.0 * beat).clamp(3.0, 10.0);
    (1.0 - next_build(an, t) / win).clamp(0.0, 1.0)
}

/// Local player's Rally input from keyboard / mouse. `aim` is a direction from the avatar.
fn rally_input(rend: &Renderer, me_pos: Vec2) -> RallyInput {
    let k = |a: KeyCode, b: KeyCode| (is_key_down(a) || is_key_down(b)) as i32 as f32;
    let mv = vec2(k(KeyCode::D, KeyCode::Right) - k(KeyCode::A, KeyCode::Left), k(KeyCode::S, KeyCode::Down) - k(KeyCode::W, KeyCode::Up));
    let (mx, my) = mouse_position();
    let aim = rend.view.to_world(vec2(mx, my)) - me_pos;
    RallyInput { mv, aim, swing: is_mouse_button_down(MouseButton::Left) || is_key_down(KeyCode::Space) }
}

impl App {
    fn lan_open(&mut self) {
        self.lan = Some(Box::new(Lan::new(self.args.selftest.as_deref())));
        self.screen = Screen::Lan;
    }

    /// Leave the LAN session (dropping `Net` tells the other side) and go back to the menu.
    fn lan_leave(&mut self, why: Option<String>) {
        if let Some(mut lan) = self.lan.take() {
            lan.note(format!("leave: {}", why.as_deref().unwrap_or("by choice")));
        }
        self.to_menu();
        if let Some(w) = why {
            self.say(w);
        }
    }

    fn lan_start_host(&mut self, lan: &mut Lan, song: Option<PathBuf>) {
        match net::Net::host(&net::local_name(), net::TCP_PORT, true) {
            Ok(n) => {
                let port = n.port;
                lan.host = true;
                lan.net = Some(n);
                lan.disc = None;
                lan.stage = LanStage::Lobby;
                lan.my_ip = net::local_ip().map(|i| i.to_string()).unwrap_or_else(|| "unknown".into());
                let song = song.or_else(|| self.juke.list.get(self.juke.idx).cloned());
                lan.set_song(song);
                lan.note(format!("hosting port={port} song='{}'", lan.song_title));
            }
            Err(e) => {
                lan.note(format!("host failed: {e}"));
                self.say(e);
            }
        }
    }

    fn lan_start_join(&mut self, lan: &mut Lan, addr: std::net::SocketAddr) {
        lan.host = false;
        lan.net = Some(net::Net::join(addr, &net::local_name()));
        lan.disc = None;
        lan.stage = LanStage::Lobby;
        lan.note(format!("joining {addr}"));
    }

    /// Command-line LAN start (`--lan-host FILE` / `--lan-join IP`).
    fn lan_cli(&mut self) {
        self.lan_open();
        let Some(mut lan) = self.lan.take() else { return };
        if let Some(f) = self.args.rally_local.clone() {
            lan.host = true;
            lan.local = true;
            lan.stage = LanStage::Lobby;
            lan.set_song(Some(f));
            lan.note("local practice vs bot".into());
        } else if let Some(f) = self.args.lan_host.clone() {
            self.lan_start_host(&mut lan, Some(f));
        } else if let Some(ip) = self.args.lan_join.clone() {
            match net::parse_addr(&ip) {
                Some(a) => self.lan_start_join(&mut lan, a),
                None => self.say(format!("not an IP address: {ip}")),
            }
        }
        self.lan = Some(lan);
    }

    /// Make sure `path` is analysed (reusing the menu's copy when it is the same track).
    fn lan_load(&mut self, lan: &mut Lan, path: PathBuf) {
        if lan.loaded.as_ref().map(|l| &l.0) == Some(&path) || lan.loader.as_ref().map(|l| &l.0) == Some(&path) {
            return;
        }
        if let Some((p, l)) = &self.juke.loaded {
            if *p == path {
                let level = level::generate(&l.an, self.args.seed, self.diff());
                lan.loaded = Some((path, Loaded { track: l.track.clone(), an: l.an.clone(), level }));
                lan.loader = None;
                return;
            }
        }
        lan.loader = Some((path.clone(), spawn_loader(0, path, self.args.seed, self.diff())));
    }

    /// Per-frame LAN housekeeping on every LAN screen: network events, song loading, start.
    fn lan_poll(&mut self, dt: f32) {
        let Some(mut lan) = self.lan.take() else { return };
        let mut leave: Option<String> = None;
        if let Some(d) = &mut lan.disc {
            d.poll(dt);
        }
        while leave.is_none() {
            let Some(ev) = lan.net.as_ref().and_then(|n| n.poll()) else { break };
            match ev {
                net::Event::Status(s) => lan.status = s,
                net::Event::Connected(name) => {
                    lan.connected = true;
                    lan.status = format!("connected to {name}");
                    lan.note(format!("connected peer={name}"));
                    lan.peer = name;
                    if self.args.rally_bot {
                        lan.my_char = if lan.host { 1 } else { 3 };
                        lan.my_ready = true;
                        lan.note(format!("ready char={}", lan.my_char));
                    }
                    lan.send_lobby();
                }
                net::Event::Msg(m) => self.lan_msg(&mut lan, m, &mut leave),
                net::Event::SongProgress(d, t) => lan.xfer = (d, t),
                net::Event::SongSaved { path, title, theme } => {
                    let p = path.or_else(|| ((theme as usize) < synth::THEME_COUNT).then(|| theme_path(theme as usize)));
                    match p {
                        Some(p) => {
                            lan.note(format!("song received '{title}'"));
                            lan.song_title = title;
                            lan.song = Some(p.clone());
                            lan.loaded = None;
                            lan.loader = None;
                            lan.told_loaded = false;
                            self.lan_load(&mut lan, p);
                        }
                        None => leave = Some("the host picked a song this version does not have".into()),
                    }
                }
                net::Event::SongSent => {
                    lan.song_sent = true;
                    lan.note(format!("song sent bytes={}", lan.xfer.1));
                }
                net::Event::Clock { offset, rtt, samples } => lan.clock = (offset, rtt, samples),
                net::Event::Disconnected(why) => leave = Some(why),
            }
        }
        // song analysis
        let mut done = None;
        if let Some((p, l)) = &lan.loader {
            if let Ok(r) = l.rx.try_recv() {
                done = Some((p.clone(), r));
            }
        }
        if let Some((p, r)) = done {
            lan.loader = None;
            match r {
                Ok(l) => {
                    lan.note(format!("song analysed beats={} bpm={:.1}", l.an.beats.len(), l.an.bpm));
                    lan.loaded = Some((p, l));
                }
                Err(e) if lan.host => {
                    self.say(format!("cannot read that song: {e}"));
                    lan.set_song(None);
                    lan.my_ready = false;
                    lan.send_lobby();
                }
                Err(e) => leave = Some(format!("could not read the host's song: {e}")),
            }
        }
        if let Some(why) = leave {
            self.lan = Some(lan);
            self.lan_leave(Some(why));
            return;
        }
        if lan.host {
            if let Some(s) = lan.song.clone() {
                self.lan_load(&mut lan, s);
            }
            // The song goes out once the host locks in with Ready.
            if lan.connected && lan.my_ready && lan.sent != lan.song {
                if let (Some(s), Some(n)) = (lan.song.clone(), &lan.net) {
                    match theme_index(&s) {
                        Some(i) => n.send_song(None, &lan.song_title, i as u8, ""),
                        None => {
                            let ext = s.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                            n.send_song(Some(s.clone()), &lan.song_title, net::NO_THEME, &ext);
                        }
                    }
                    lan.sent = Some(s);
                    lan.song_sent = false;
                    lan.peer_loaded = false;
                    lan.xfer = (0, 0);
                    lan.note(format!("sending song '{}'", lan.song_title));
                }
            }
            if lan.local && lan.stage == LanStage::Lobby && lan.is_loaded() {
                let seed = if self.args.seed != 0 { self.args.seed } else { 1 };
                let chars = [lan.my_char, 2];
                self.lan_begin(&mut lan, net::now() + LAN_COUNTDOWN, seed, chars);
            }
            let go = lan.stage == LanStage::Lobby
                && lan.connected
                && lan.my_ready
                && lan.peer_ready
                && lan.song_sent
                && lan.peer_loaded
                && lan.sent == lan.song
                && lan.is_loaded();
            if go {
                let seed = if self.args.seed != 0 {
                    self.args.seed
                } else {
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(9)
                };
                let chars = [lan.my_char, lan.peer_char];
                let host_time = net::now() + LAN_COUNTDOWN;
                lan.send(&net::Msg::Start { host_time, seed, chars });
                lan.note(format!("start host_time={host_time:.3} seed={seed} chars={chars:?}"));
                self.lan_begin(&mut lan, host_time, seed, chars);
            }
        } else if !lan.told_loaded && lan.is_loaded() && lan.clock.2 >= net::SYNC_SAMPLES as u32 {
            lan.send(&net::Msg::Loaded);
            lan.told_loaded = true;
            lan.note(format!("loaded; clock offset={:.4}s rtt={:.2}ms", lan.clock.0, lan.clock.1 * 1000.0));
        }
        if lan.stage == LanStage::Results {
            lan.result_t += dt;
            if self.args.rally_bot && self.args.selftest.is_none() && lan.result_t > 5.0 {
                lan_to_lobby(&mut lan, true);
            }
        }
        self.lan = Some(lan);
    }

    fn lan_msg(&mut self, lan: &mut Lan, m: net::Msg, leave: &mut Option<String>) {
        use net::Msg;
        match m {
            Msg::Lobby { ch, ready } => {
                let ch = ch.min(Character::ALL.len() as u8 - 1);
                if (ch, ready) != (lan.peer_char, lan.peer_ready) {
                    lan.note(format!("peer char={ch} ready={ready}"));
                }
                lan.peer_char = ch;
                lan.peer_ready = ready;
            }
            Msg::Loaded if lan.host => {
                lan.peer_loaded = true;
                lan.note("peer loaded".into());
            }
            Msg::Start { host_time, seed, chars } if !lan.host => {
                if !lan.is_loaded() {
                    *leave = Some("the host started before the song was ready".into());
                    return;
                }
                let local = host_time - lan.clock.0;
                lan.note(format!(
                    "start host_time={host_time:.3} local={local:.3} in={:.3}s offset={:.4} rtt={:.2}ms seed={seed} chars={chars:?}",
                    local - net::now(),
                    lan.clock.0,
                    lan.clock.1 * 1000.0
                ));
                self.lan_begin(lan, local, seed, chars);
            }
            Msg::Input(i) if lan.host => {
                if let Some(m) = &mut lan.m {
                    if i.seq > m.peer_seq {
                        m.peer_seq = i.seq;
                        m.peer_in = RallyInput { mv: vec2(i.mv[0], i.mv[1]), aim: vec2(i.aim[0], i.aim[1]), swing: false };
                        if i.swings != m.peer_swings {
                            m.peer_swings = i.swings;
                            m.peer_pulse = true;
                        }
                    }
                }
            }
            Msg::Snap { t, ack, data } if !lan.host => {
                if let Some(m) = &mut lan.m {
                    std::mem::swap(&mut m.prev, &mut m.rally);
                    if m.rally.decode(&data) {
                        m.prev_at = m.cur_at;
                        m.cur_at = net::now();
                        m.snaps += 1;
                        m.snap_t = t;
                        m.ack = ack;
                        while m.hist.front().is_some_and(|h| h.0 <= ack) {
                            m.hist.pop_front();
                        }
                        if m.snaps == 1 {
                            m.prev.clone_from(&m.rally);
                        }
                    } else {
                        std::mem::swap(&mut m.prev, &mut m.rally);
                        m.bad_snaps += 1;
                    }
                }
                if let Some(n) = &lan.net {
                    n.recycle(data);
                }
            }
            Msg::End { score } if !lan.host => self.lan_end(lan, score),
            Msg::Snap { data, .. } => {
                if let Some(n) = &lan.net {
                    n.recycle(data);
                }
            }
            _ => {}
        }
    }

    /// Countdown: both PCs start the song at `start_local` on their own clock.
    fn lan_begin(&mut self, lan: &mut Lan, start_local: f64, seed: u64, chars: [u8; 2]) {
        let Some((_, l)) = &lan.loaded else { return };
        let ch = |i: u8| Character::from_index(i).unwrap_or(Character::Sub);
        let rally = Rally::new(seed, [ch(chars[0]), ch(chars[1])]);
        let an = l.an.clone();
        let bars: Vec<f32> = if an.beats.is_empty() { Vec::new() } else { an.beats[an.downbeat.min(an.beats.len() - 1)..].iter().step_by(4).copied().collect() };
        let beat_len = if l.level.bpm > 1.0 { 60.0 / l.level.bpm } else { 0.5 };
        let traffic0 = lan.net.as_ref().map(|n| n.traffic()).unwrap_or_default();
        let m = LanMatch {
            track: l.track.clone(),
            events: l.level.events.clone(),
            beats: an.beats.clone(),
            bars,
            duration: an.duration.max(1.0),
            an,
            cursor: 0,
            t: (net::now() - start_local) as f32,
            start_local,
            audio_on: false,
            prev: rally.clone(),
            view: rally.clone(),
            rally,
            prev_at: 0.0,
            cur_at: 0.0,
            snaps: 0,
            bad_snaps: 0,
            snap_t: 0.0,
            buf: Vec::with_capacity(1024),
            send_acc: 0.0,
            peer_in: RallyInput::default(),
            peer_swings: 0,
            peer_pulse: false,
            peer_seq: 0,
            seq: 0,
            swings: 0,
            down: false,
            ack: 0,
            hist: VecDeque::with_capacity(LAN_HIST),
            fx_seen: -1.0e9,
            score_seen: [0; 2],
            confirm: false,
            traffic0,
            wall0: net::now(),
        };
        lan.m = Some(Box::new(m));
        lan.stage = LanStage::Countdown;
        self.juke.playing = false;
        self.audio.fade_out(0.4);
        self.world = World::new(self.diff(), true);
        self.world.beat_len = beat_len;
    }

    fn lan_end(&mut self, lan: &mut Lan, score: [u32; 2]) {
        lan.result = score;
        lan.result_t = 0.0;
        lan.stage = LanStage::Results;
        lan.my_ready = false;
        lan.peer_ready = false;
        self.audio.fade_out(1.0);
        if let Some(m) = lan.m.take() {
            let secs = (net::now() - m.wall0).max(0.001);
            let (s0, r0) = m.traffic0;
            let (s1, r1) = lan.net.as_ref().map(|n| n.traffic()).unwrap_or((s0, r0));
            lan.note(format!(
                "match over t={:.2} snapshot_score={}-{} snaps={} bad_snaps={} up={:.1}KB/s down={:.1}KB/s",
                m.t,
                m.rally.score()[0],
                m.rally.score()[1],
                m.snaps,
                m.bad_snaps,
                (s1 - s0) as f64 / secs / 1024.0,
                (r1 - r0) as f64 / secs / 1024.0
            ));
        }
        lan.note(format!("final score {}-{}", score[0], score[1]));
        lan.send_lobby();
    }

    /// Countdown and match: host steps Rally, client predicts + interpolates. Returns progress.
    fn lan_step(&mut self, dt: f32) -> f32 {
        let Some(mut lan) = self.lan.take() else { return 0.0 };
        let host = lan.host;
        let me = if host { 0 } else { 1 };
        let now = net::now();
        let mut notes: [Option<String>; 2] = [None, None];
        let mut end_score = None;
        let progress;
        {
            let Some(mb) = lan.m.as_mut() else {
                self.lan = Some(lan);
                return 0.0;
            };
            let m: &mut LanMatch = mb;
            if !m.audio_on && now >= m.start_local {
                let late = now - m.start_local;
                self.audio.set_volume(if self.args.mute { 0.0 } else { self.volume });
                self.audio.play_ex(m.track.clone(), late, false, 0.0);
                m.audio_on = true;
                notes[0] = Some(format!("audio start late={:.2}ms", late * 1000.0));
            }
            let t = if m.audio_on {
                let a = self.audio.song_time() as f32 - self.offset_ms / 1000.0;
                a.max(m.t).min(m.t + 0.25)
            } else {
                (now - m.start_local) as f32
            };
            m.t = t;
            let tt = t.max(0.0);
            let playing = m.audio_on && t >= 0.0;
            let section = m.an.section_at(tt).map(|s| s.kind).unwrap_or(SectionKind::Normal);
            let music = RallyMusic { beats: &m.beats, bars: &m.bars, section, next_build: build_ramp(&m.an, tt) };
            let mine = if host { &m.rally } else { &m.view };
            let mut inp = if self.args.rally_bot { mine.bot_input(me, t, &music) } else { rally_input(&self.rend, mine.players[me].pos) };
            if !playing || m.confirm {
                inp = RallyInput { aim: inp.aim, ..RallyInput::default() };
            }
            if host {
                if playing {
                    let mut pin = m.peer_in;
                    pin.swing = m.peer_pulse;
                    m.peer_pulse = false;
                    if lan.local {
                        pin = m.rally.bot_input(1, t, &music);
                    }
                    m.rally.step(dt, t, [inp, pin], &music);
                }
                m.send_acc += dt;
                let over = playing && (self.audio.finished() || t >= m.duration - 0.05);
                let test_over = self.args.selftest.is_some() && self.wall >= self.args.secs - 4.0;
                if m.send_acc >= LAN_SEND_DT || over || test_over {
                    m.send_acc = (m.send_acc - LAN_SEND_DT).clamp(0.0, LAN_SEND_DT);
                    m.buf.clear();
                    m.rally.encode(&mut m.buf);
                    if let Some(n) = &lan.net {
                        n.send_snap(t, m.peer_seq, &m.buf);
                    }
                }
                if over || test_over {
                    let score = m.rally.score();
                    if let Some(n) = &lan.net {
                        n.send(&net::Msg::End { score });
                    }
                    end_score = Some(score);
                }
            } else {
                // Swing presses are counted, so one is never lost between 60 Hz input packets.
                if inp.swing && !m.down {
                    m.swings = m.swings.wrapping_add(1);
                }
                m.down = inp.swing;
                m.send_acc += dt;
                if m.send_acc >= LAN_SEND_DT {
                    m.send_acc = (m.send_acc - LAN_SEND_DT).clamp(0.0, LAN_SEND_DT);
                    let q = |v: f32| (v.clamp(-1.0, 1.0) * 127.0).round() / 127.0;
                    let sent = RallyInput { mv: vec2(q(inp.mv.x), q(inp.mv.y)), aim: inp.aim, swing: false };
                    m.seq += 1;
                    if let Some(n) = &lan.net {
                        n.send(&net::Msg::Input(net::NetInput { seq: m.seq, mv: [sent.mv.x, sent.mv.y], aim: [sent.aim.x, sent.aim.y], swings: m.swings }));
                    }
                    if m.hist.len() >= LAN_HIST {
                        m.hist.pop_front();
                    }
                    m.hist.push_back((m.seq, sent, LAN_SEND_DT));
                }
                // Draw one snapshot behind (interpolated), with our own avatar predicted.
                if m.snaps >= 2 {
                    let span = (m.cur_at - m.prev_at).clamp(0.004, 0.1);
                    m.view.interpolate(&m.prev, &m.rally, ((now - m.cur_at) / span) as f32);
                } else {
                    m.view.clone_from(&m.rally);
                }
                if playing && m.snaps > 0 {
                    m.view.players[me] = m.rally.players[me];
                    for (s, i, d) in m.hist.iter() {
                        if *s > m.ack {
                            m.view.predict_move(me, *d, i);
                        }
                    }
                    m.view.predict_move(me, m.send_acc, &inp);
                }
            }
            // Sounds and screen kicks for new Rally events.
            let shown = if host { &m.rally } else { &m.view };
            let mut newest = m.fx_seen;
            for f in shown.events() {
                if f.t <= m.fx_seen {
                    continue;
                }
                newest = newest.max(f.t);
                let pan = (f.pos.x / level::HALF_W).clamp(-1.0, 1.0);
                match f.kind {
                    FxKind::Hit => self.audio.sfx(Sfx::NoteShot, 0.6, pan),
                    FxKind::Perfect => self.audio.sfx(Sfx::Shard, 0.9, pan),
                    FxKind::Slam => {
                        self.audio.sfx(Sfx::Slam, 1.0, pan);
                        self.world.shake = self.world.shake.max(0.6);
                    }
                    FxKind::Split => self.audio.sfx(Sfx::Kill, 0.7, pan),
                    FxKind::Score => {
                        self.audio.sfx(Sfx::HeartBreak, 0.9, pan);
                        self.world.shake = self.world.shake.max(0.8);
                        self.world.flash = self.world.flash.max(0.5);
                    }
                    FxKind::Serve => self.audio.sfx(Sfx::UiMove, 0.4, pan),
                    FxKind::Bounce => {}
                }
            }
            m.fx_seen = newest;
            let sc = m.rally.score();
            if sc != m.score_seen {
                m.score_seen = sc;
                notes[1] = Some(if host {
                    format!("point t={:.3} score {}-{}", m.rally.t, sc[0], sc[1])
                } else {
                    format!("point host_t={:.3} local_t={:.3} score {}-{}", m.snap_t, t, sc[0], sc[1])
                });
            }
            // The visual layer reacts to the music exactly like the menu: beats, hits, sections.
            while m.cursor < m.events.len() && m.events[m.cursor].at() <= tt {
                let e = m.events[m.cursor];
                m.cursor += 1;
                if matches!(e.ev, Ev::Beat { .. } | Ev::Hit { .. } | Ev::Section { .. }) {
                    self.world.apply(&e);
                }
            }
            self.world.t = tt;
            let feat = if playing { feat_at(&m.an, tt) } else { Feat::default() };
            let bp = beat_pos(&m.beats, tt);
            self.world.update(dt, tt, &Input::default(), bp, &feat, m.an.intensity_at(tt), false);
            self.world.player.pos = Vec2::ZERO;
            if m.audio_on && lan.stage == LanStage::Countdown {
                lan.stage = LanStage::Match;
            }
            progress = tt / m.duration;
        }
        for n in notes.iter_mut() {
            if let Some(s) = n.take() {
                lan.note(s);
            }
        }
        if let Some(sc) = end_score {
            self.lan_end(&mut lan, sc);
        }
        self.lan = Some(lan);
        progress
    }

    fn lan_draw(&mut self, dt: f32) {
        let Some(lan) = self.lan.as_ref() else { return };
        let Some(m) = lan.m.as_ref() else { return };
        let me = if lan.host { 0 } else { 1 };
        self.rend.fx = VISUALS[self.visuals].0;
        let t = m.t.max(0.0);
        if m.audio_on {
            self.rend.feed_signal(Some((&m.track.samples, m.track.sr)), t, next_build(&m.an, t));
        } else {
            self.rend.feed_signal(None, t, f32::INFINITY);
        }
        let r = if lan.host { &m.rally } else { &m.view };
        self.rend.draw_rally(r, &self.world, me, dt);
    }

    /// LAN screens (drawn over the scene) and their input.
    fn lan_ui(&mut self, u: f32, sw: f32, sh: f32) {
        let Some(mut lan) = self.lan.take() else {
            self.screen = Screen::Menu;
            return;
        };
        let pal = self.rend.pal;
        let x = 110.0 * u;
        let esc = is_key_pressed(KeyCode::Escape);
        let enter = is_key_pressed(KeyCode::Enter) || is_key_pressed(KeyCode::KpEnter);
        let mut leave: Option<Option<String>> = None;
        let soft = Color::new(1.0, 1.0, 1.0, 0.7);
        match lan.stage {
            LanStage::Choose => {
                dim(0.6);
                render::vtext("LAN RALLY", x, 130.0 * u, 64.0 * u, WHITE, -1);
                text_left("A beat-locked paddle duel for two PCs on the same network.", x, 175.0 * u, 24.0 * u, pal.primary);
                let song = self.juke.list.get(self.juke.idx).map(|p| track_title(p)).unwrap_or_default();
                let items = vec![
                    format!("HOST A MATCH   ({song})"),
                    "JOIN A MATCH".to_string(),
                    format!("PRACTICE VS BOT   ({song})"),
                    "BACK".to_string(),
                ];
                let act = menu_list(&items, &mut lan.sel, x, 260.0 * u, u, &self.rend, &self.audio);
                let fy = sh - 150.0 * u;
                text_left("The first time you host, Windows Firewall asks whether Heartline may use the network.", x, fy, 22.0 * u, pal.accent);
                text_left("Allow it on Private networks. Heartline never changes firewall settings itself.", x, fy + 28.0 * u, 22.0 * u, soft);
                text_left("Both PCs must be on the same network. Ports: TCP 47820 (game), UDP 47821 (finding hosts).", x, fy + 56.0 * u, 22.0 * u, soft);
                match act {
                    Some(0) => self.lan_start_host(&mut lan, None),
                    Some(1) => {
                        lan.stage = LanStage::Join;
                        lan.sel = 0;
                        lan.disc = Some(net::Discovery::start());
                    }
                    Some(2) => {
                        // Offline practice against the bot on the menu's current track.
                        let cur = self.juke.list.get(self.juke.idx).cloned();
                        if cur.is_some() {
                            lan.host = true;
                            lan.local = true;
                            lan.stage = LanStage::Lobby;
                            lan.set_song(cur);
                            lan.note("local practice vs bot".into());
                        } else {
                            self.say("no track selected: pick one in the main menu first");
                        }
                    }
                    Some(3) => leave = Some(None),
                    _ => {}
                }
                if esc {
                    leave = Some(None);
                }
            }
            LanStage::Join => {
                dim(0.6);
                render::vtext("JOIN A MATCH", x, 130.0 * u, 64.0 * u, WHITE, -1);
                let (hosts, err) = match &lan.disc {
                    Some(d) => (d.hosts.clone(), d.error.clone()),
                    None => (Vec::new(), None),
                };
                let msg = err.unwrap_or_else(|| if hosts.is_empty() { "looking for hosts on your network...".into() } else { "hosts on your network:".into() });
                text_left(&msg, x, 175.0 * u, 24.0 * u, pal.primary);
                while let Some(c) = get_char_pressed() {
                    if (c.is_ascii_digit() || c == '.' || c == ':') && lan.ip.len() < 21 {
                        lan.ip.push(c);
                    }
                }
                if is_key_pressed(KeyCode::Backspace) {
                    lan.ip.pop();
                }
                let mut items: Vec<String> = hosts
                    .iter()
                    .map(|h| format!("{}   {}{}", h.name, h.addr.ip(), if h.compatible { "" } else { "   (different version)" }))
                    .collect();
                items.push(format!("TYPE AN IP:   {}_", lan.ip));
                items.push("BACK".to_string());
                lan.sel = lan.sel.min(items.len() - 1);
                let act = menu_list(&items, &mut lan.sel, x, 230.0 * u, u, &self.rend, &self.audio);
                text_left("type the host's IP (shown on its lobby screen) and press ENTER on that row", x, sh - 90.0 * u, 22.0 * u, soft);
                let nh = hosts.len();
                match act {
                    Some(i) if i < nh => {
                        if hosts[i].compatible {
                            self.lan_start_join(&mut lan, hosts[i].addr);
                        } else {
                            self.say("that host is on a different version of Heartline");
                        }
                    }
                    Some(i) if i == nh => match net::parse_addr(&lan.ip) {
                        Some(a) => self.lan_start_join(&mut lan, a),
                        None => self.say("type an IP like 192.168.1.20"),
                    },
                    Some(_) => {
                        lan.stage = LanStage::Choose;
                        lan.disc = None;
                    }
                    None => {}
                }
                if esc {
                    lan.stage = LanStage::Choose;
                    lan.disc = None;
                }
            }
            LanStage::Lobby => {
                dim(0.62);
                render::vtext(if lan.host { "HOSTING" } else { "LOBBY" }, x, 130.0 * u, 64.0 * u, WHITE, -1);
                let line = if lan.host && !lan.connected {
                    format!("your IP  {}   -   waiting for a player to join", lan.my_ip)
                } else if lan.connected {
                    format!("{}   -   ping {:.1} ms", lan.status, lan.clock.1 * 1000.0)
                } else {
                    lan.status.clone()
                };
                text_left(&line, x, 175.0 * u, 24.0 * u, pal.primary);
                // player cards
                let peer_name = if lan.connected { lan.peer.clone() } else { "waiting...".to_string() };
                let cards = [("YOU", net::local_name(), lan.my_char, lan.my_ready, true), ("THEM", peer_name, lan.peer_char, lan.peer_ready, lan.connected)];
                for (k, (who, name, ch, ready, on)) in cards.iter().enumerate() {
                    let cx = x + k as f32 * 600.0 * u;
                    let cy = 220.0 * u;
                    draw_rectangle(cx, cy, 560.0 * u, 170.0 * u, Color::new(0.0, 0.0, 0.05, 0.5));
                    draw_rectangle_lines(cx, cy, 560.0 * u, 170.0 * u, 2.0 * u, if *ready { pal.accent } else { Color::new(1.0, 1.0, 1.0, 0.25) });
                    text_left(&format!("{who}   {name}"), cx + 20.0 * u, cy + 36.0 * u, 22.0 * u, soft);
                    if *on {
                        let c = Character::from_index(*ch).unwrap_or(Character::Sub);
                        let label = if k == 0 && !*ready { format!("<  {}  >", c.name()) } else { c.name().to_string() };
                        render::vtext(&label, cx + 20.0 * u, cy + 92.0 * u, 44.0 * u, WHITE, -1);
                        text_left(CHAR_DESC[(*ch as usize).min(3)], cx + 20.0 * u, cy + 126.0 * u, 20.0 * u, pal.primary);
                    }
                    text_left(if *ready { "READY" } else { "not ready" }, cx + 20.0 * u, cy + 156.0 * u, 22.0 * u, if *ready { pal.accent } else { soft });
                }
                // song + progress
                let mut y = 440.0 * u;
                let title = if lan.song_title.is_empty() { "(none)".to_string() } else { lan.song_title.clone() };
                text_left(&format!("SONG   {title}"), x, y, 28.0 * u, WHITE);
                if lan.host && !lan.my_ready {
                    text_left("Q / E  change track      O  open a file", x + 760.0 * u, y, 22.0 * u, soft);
                }
                let bar = |y: f32, label: &str, f: f32| {
                    let w = 700.0 * u;
                    text_left(label, x, y, 22.0 * u, soft);
                    draw_rectangle(x + 300.0 * u, y - 16.0 * u, w, 18.0 * u, Color::new(0.0, 0.0, 0.05, 0.6));
                    draw_rectangle(x + 300.0 * u, y - 16.0 * u, w * f.clamp(0.0, 1.0), 18.0 * u, pal.secondary);
                };
                y += 44.0 * u;
                if lan.xfer.1 > 0 {
                    let f = lan.xfer.0 as f32 / lan.xfer.1 as f32;
                    let label = format!("{}  {:.1} / {:.1} MB", if lan.host { "SENDING SONG" } else { "RECEIVING SONG" }, lan.xfer.0 as f32 / 1048576.0, lan.xfer.1 as f32 / 1048576.0);
                    bar(y, &label, f);
                    y += 36.0 * u;
                }
                if let Some((_, l)) = &lan.loader {
                    let p = l.progress.load(Ordering::Relaxed) as f32 / 1000.0;
                    let label = if l.stage.load(Ordering::Relaxed) == 0 { "DECODING" } else { "READING THE MUSIC" };
                    bar(y, label, p);
                    y += 36.0 * u;
                } else if lan.is_loaded() {
                    text_left("song ready on this PC", x, y, 22.0 * u, pal.accent);
                    y += 36.0 * u;
                }
                if lan.host && lan.connected {
                    let s = if !lan.my_ready {
                        "press ENTER when ready: the song is sent to the other player"
                    } else if lan.peer_loaded {
                        "their copy is ready"
                    } else {
                        "their PC is getting the song ready..."
                    };
                    text_left(s, x, y, 22.0 * u, soft);
                }
                text_left("LEFT / RIGHT  character      ENTER  ready      ESC  leave", x, sh - 90.0 * u, 24.0 * u, pal.accent);
                // input
                if !lan.my_ready {
                    let d = is_key_pressed(KeyCode::Right) as i32 - is_key_pressed(KeyCode::Left) as i32;
                    if d != 0 {
                        lan.my_char = (lan.my_char as i32 + d).rem_euclid(Character::ALL.len() as i32) as u8;
                        self.audio.sfx(Sfx::UiMove, 0.6, 0.0);
                        lan.send_lobby();
                    }
                    if lan.host {
                        let td = is_key_pressed(KeyCode::E) as i32 - is_key_pressed(KeyCode::Q) as i32;
                        if td != 0 {
                            self.juke_next(td);
                            let p = self.juke.list.get(self.juke.idx).cloned();
                            lan.set_song(p);
                        }
                        if is_key_pressed(KeyCode::O) && lan.dialog.is_none() {
                            let (tx, rx) = channel();
                            std::thread::spawn(move || {
                                let exts: Vec<&str> = decode::AUDIO_EXTS.to_vec();
                                let r = rfd::FileDialog::new().set_title("Heartline - choose the Rally song").add_filter("Audio", &exts).pick_file();
                                let _ = tx.send(r.into_iter().collect());
                            });
                            lan.dialog = Some(rx);
                        }
                    }
                }
                let picked = lan.dialog.as_ref().and_then(|rx| match rx.try_recv() {
                    Ok(p) => Some(p),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Vec::new()),
                    Err(_) => None,
                });
                if let Some(p) = picked {
                    lan.dialog = None;
                    if let Some(f) = p.into_iter().next() {
                        lan.set_song(Some(f));
                    }
                }
                if enter {
                    if lan.host && lan.song.is_none() {
                        self.say("pick a song first (Q / E or O)");
                    } else {
                        lan.my_ready = !lan.my_ready;
                        self.audio.sfx(Sfx::UiSelect, 0.8, 0.0);
                        lan.note(format!("ready={} char={}", lan.my_ready, lan.my_char));
                        lan.send_lobby();
                    }
                }
                if esc {
                    leave = Some(None);
                }
            }
            LanStage::Countdown | LanStage::Match => {
                let mut confirm_leave = false;
                if let Some(m) = &mut lan.m {
                    let left = m.start_local - net::now();
                    if left > 0.0 {
                        let n = ["1", "2", "3"][(left.ceil() as usize).clamp(1, 3) - 1];
                        render::vtext(n, sw * 0.5, sh * 0.45, 180.0 * u, WHITE, 0);
                    } else if left > -0.6 {
                        render::vtext("GO", sw * 0.5, sh * 0.45, 180.0 * u, pal.accent, 0);
                    }
                    if esc {
                        m.confirm = !m.confirm;
                    } else if m.confirm {
                        dim(0.55);
                        render::vtext("LEAVE MATCH?", sw * 0.5, sh * 0.4, 72.0 * u, WHITE, 0);
                        text_center("there is no pause in LAN play", sw * 0.5, sh * 0.47, 26.0 * u, soft);
                        text_center("ENTER  leave          ESC  keep playing", sw * 0.5, sh * 0.54, 28.0 * u, pal.accent);
                        confirm_leave = enter;
                    }
                }
                if confirm_leave {
                    leave = Some(None);
                }
            }
            LanStage::Results => {
                dim(0.6);
                let me = if lan.host { 0 } else { 1 };
                let (a, b) = (lan.result[me], lan.result[1 - me]);
                let (head, col) = if a > b {
                    ("YOU WIN", pal.accent)
                } else if a < b {
                    ("YOU LOSE", pal.danger)
                } else {
                    ("DRAW", WHITE)
                };
                render::vtext(head, sw * 0.5, sh * 0.3, 84.0 * u, col, 0);
                text_center(&format!("{a}   :   {b}"), sw * 0.5, sh * 0.44, 96.0 * u, WHITE);
                text_center(&format!("you vs {}", lan.peer), sw * 0.5, sh * 0.52, 28.0 * u, soft);
                text_center("ENTER  back to the lobby          ESC  leave", sw * 0.5, sh * 0.7, 28.0 * u, pal.accent);
                if enter {
                    lan_to_lobby(&mut lan, false);
                } else if esc {
                    leave = Some(None);
                }
            }
        }
        self.lan = Some(lan);
        if let Some(why) = leave {
            self.lan_leave(why);
        }
    }
}

fn lan_to_lobby(lan: &mut Lan, ready: bool) {
    lan.stage = LanStage::Lobby;
    lan.my_ready = ready;
    lan.note("back to lobby".into());
    lan.send_lobby();
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
        music_gain: MENU_MUSIC_GAIN,
        offset_ms: 0.0,
        music_dir: None,
        juke: Jukebox { list: Vec::new(), idx: 0, loaded: None, loader: None, cursor: 0, last_t: -1.0, playing: false, fails: 0 },
        autofire: true,
        fullscreen: false,
        visuals: 0,
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
        lan: None,
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
    app.visuals = (getf("visuals", 0.0).max(0.0) as usize).min(VISUALS.len() - 1);
    // Borderless fullscreen is remembered between launches (never for bot or sized test runs).
    if args.selftest.is_none() && args.size.is_none() && getf("fullscreen", 0.0) > 0.5 {
        app.fullscreen = true;
        display::set_borderless(true);
    }
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
    } else if args.lan_host.is_some() || args.lan_join.is_some() || args.rally_local.is_some() {
        app.lan_cli();
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
            display::set_borderless(app.fullscreen);
            app.save_settings();
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
        if app.screen == Screen::Lan {
            app.lan_poll(dt);
        }
        let lan_match = app.screen == Screen::Lan && app.lan.as_ref().is_some_and(|l| l.in_match());
        app.rend.update_view(&app.world);

        // ---- simulate ----
        let bot = app.args.selftest.is_some();
        let mut hud = HudInfo::empty();
        let mut idle_target = 1.0;
        let mut progress = 0.0;
        match app.screen {
            Screen::Lan if lan_match => {
                idle_target = 0.0;
                progress = app.lan_step(dt);
            }
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
        if lan_match && app.lan.as_ref().is_some_and(|l| l.in_match()) {
            // Rally draws its own field and score HUD over the music-reactive scene.
            app.lan_draw(dt);
        } else {
            app.feed_signal();
            // Menu-like screens: no player or Heart in the vista, and the music sits lower than in a run.
            let in_run = matches!(app.screen, Screen::Play | Screen::Paused);
            app.rend.menu = !in_run;
            let duck_to = if in_run { if matches!(app.phase, Phase::Dying(_)) { 0.35 } else { 1.0 } } else { MENU_MUSIC_GAIN };
            app.music_gain += (duck_to - app.music_gain).clamp(-1.5 * dt, 1.5 * dt);
            app.audio.set_volume(if app.args.mute { 0.0 } else { app.volume * app.music_gain });
            app.rend.draw_scene(&app.world, dt, app.idle, progress);
            app.rend.draw_hud(&app.world, &hud, app.idle);
        }
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
                    "LAN RALLY   (two PCs, same network)".to_string(),
                    format!("DIFFICULTY   {}", DIFFS[app.diff].1),
                    "MUSIC VOLUME".to_string(),
                    "EFFECTS VOLUME".to_string(),
                    format!("SYNC OFFSET   {:+.0} ms", app.offset_ms),
                    format!("VISUALS   {}", VISUALS[app.visuals].1),
                    format!("DISPLAY   {}", if app.fullscreen { "BORDERLESS FULLSCREEN" } else { "WINDOWED" }),
                    "QUIT".to_string(),
                ];
                let act = if app.dialog.is_none() { menu_list(&items, &mut app.menu_sel, 110.0 * u, 256.0 * u, u, &app.rend, &app.audio) } else { None };
                // Volume rows are sliders: drag with the mouse, or Left / Right.
                let act = match act {
                    Some(8) | Some(9) => None,
                    a => a,
                };
                let mut slid = false;
                for (row, is_sfx) in [(8usize, false), (9usize, true)] {
                    let by = 256.0 * u + row as f32 * 54.0 * u;
                    let (tx, tw) = (110.0 * u + 262.0 * u, 176.0 * u);
                    let ty = by + 23.0 * u;
                    let v = if is_sfx { app.sfx_volume } else { app.volume };
                    let on = app.menu_sel == row;
                    let col = if on { pal.accent } else { pal.primary };
                    draw_rectangle(tx, ty - 1.5 * u, tw, 3.0 * u, Color::new(1.0, 1.0, 1.0, 0.22));
                    draw_rectangle(tx, ty - 1.5 * u, tw * v, 3.0 * u, col);
                    draw_circle(tx + tw * v, ty, if on { 8.0 } else { 6.0 } * u, WHITE);
                    draw_circle_lines(tx + tw * v, ty, if on { 11.0 } else { 9.0 } * u, 1.5 * u, col);
                    text_left(&format!("{:.0}%", v * 100.0), tx + tw + 20.0 * u, by + 31.0 * u, 22.0 * u, Color::new(1.0, 1.0, 1.0, 0.85));
                    let (mx, my) = mouse_position();
                    if app.dialog.is_none() && is_mouse_button_down(MouseButton::Left) && my >= by && my <= by + 46.0 * u && mx >= tx - 14.0 * u && mx <= tx + tw + 14.0 * u {
                        let nv = (((mx - tx) / tw).clamp(0.0, 1.0) * 20.0).round() / 20.0;
                        if (nv - v).abs() > 0.001 {
                            if is_sfx { app.sfx_volume = nv } else { app.volume = nv }
                            app.menu_sel = row;
                            slid = true;
                        }
                    }
                }
                // left/right adjusts the selected row; on the play rows it changes the track
                let dirk = is_key_pressed(KeyCode::Right) as i32 - is_key_pressed(KeyCode::Left) as i32;
                let track_dirk = is_key_pressed(KeyCode::E) as i32 - is_key_pressed(KeyCode::Q) as i32 + if app.menu_sel <= 1 { dirk } else { 0 };
                if track_dirk != 0 && app.dialog.is_none() {
                    app.juke_next(track_dirk);
                }
                let mut changed = slid;
                if dirk != 0 {
                    changed = true;
                    match app.menu_sel {
                        7 => app.diff = ((app.diff as i32 + dirk).rem_euclid(3)) as usize,
                        8 => app.volume = (app.volume + 0.1 * dirk as f32).clamp(0.0, 1.0),
                        9 => app.sfx_volume = (app.sfx_volume + 0.1 * dirk as f32).clamp(0.0, 1.0),
                        10 => app.offset_ms = (app.offset_ms + 10.0 * dirk as f32).clamp(-100.0, 300.0),
                        11 => app.visuals = ((app.visuals as i32 + dirk).rem_euclid(VISUALS.len() as i32)) as usize,
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
                    Some(6) => app.lan_open(),
                    Some(7) => {
                        app.diff = (app.diff + 1) % 3;
                        changed = true;
                    }
                    Some(8) => {
                        app.volume = if app.volume >= 0.99 { 0.0 } else { (app.volume + 0.1).min(1.0) };
                        changed = true;
                    }
                    Some(9) => {
                        app.sfx_volume = if app.sfx_volume >= 0.99 { 0.0 } else { (app.sfx_volume + 0.1).min(1.0) };
                        changed = true;
                    }
                    Some(10) => {
                        app.offset_ms = if app.offset_ms >= 295.0 { -100.0 } else { app.offset_ms + 10.0 };
                        changed = true;
                    }
                    Some(11) => {
                        app.visuals = (app.visuals + 1) % VISUALS.len();
                        changed = true;
                    }
                    Some(12) => {
                        app.fullscreen = !app.fullscreen;
                        display::set_borderless(app.fullscreen);
                        changed = true;
                    }
                    Some(13) => break,
                    _ => {}
                }
                if changed {
                    let mute = app.args.mute;
                    app.audio.set_sfx_volume(if mute { 0.0 } else { app.sfx_volume });
                    if app.menu_sel == 9 && !is_mouse_button_down(MouseButton::Left) || (slid && is_mouse_button_pressed(MouseButton::Left)) {
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
                    11 => "VISUALS: how hard the scene pulses with the music (zoom punches, flashes, rings). SUBTLE or OFF for comfort",
                    6 => "LAN RALLY: a beat-locked paddle duel against a second PC on your network; the host picks the song",
                    10 => "SYNC OFFSET: raise it if attacks land before you hear the beat (soundbar / Bluetooth lag), lower it if they land late",
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
            Screen::Lan => app.lan_ui(u, sw, sh),
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
            // A LAN client waits (a little) for the host's End so both logs hold the final score.
            let lan_wait = app.lan.as_ref().is_some_and(|l| !l.host && l.in_match()) && app.wall < app.args.secs + 10.0;
            let over = (app.wall >= app.args.secs && !lan_wait) || app.screen == Screen::Results && app.wall > 3.0 && app.shots_done >= app.args.shots.len();
            if over {
                if let Some(lan) = app.lan.as_mut() {
                    let stage = lan.stage;
                    lan.note(format!("selftest end stage={stage:?} result={}-{}", lan.result[0], lan.result[1]));
                }
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
