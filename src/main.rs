//! Heartline — your music becomes a top-down vaporwave bullet-hell.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analysis;
mod decode;
mod engine;
mod game;
mod level;
mod render;
mod rng;
mod tools;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use macroquad::prelude::*;

use analysis::{Analysis, Band, Feat, StreamAnalyzer, StreamOut};
use decode::Track;
use engine::Audio;
use game::{Input, UpgradeId, World};
use level::{Ev, Level, LevelEvent, LiveGen};
use render::{spaced, text_center, text_left, text_shadow, text_w, HudInfo, Renderer};

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
    screen: String,
    live_in: String,
    live_out: String,
}

fn parse_args() -> Args {
    let mut a = Args { diff: 1, secs: 30.0, ..Default::default() };
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
            "--diff" => {
                a.diff = match val().to_ascii_lowercase().as_str() {
                    "chill" | "0" => 0,
                    "hard" | "2" => 2,
                    _ => 1,
                }
            }
            "--mute" => a.mute = true,
            "--wall" => a.wall = true,
            "--mortal" => a.mortal = true,
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
    let cli = args.analyze.is_some() || args.gen_test.is_some() || args.live_probe.is_some();
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
    let track = decode::decode_file(path, progress)?;
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
    autofire: bool,
    fullscreen: bool,
    menu_sel: usize,
    dialog: Option<Receiver<Vec<PathBuf>>>,
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

fn track_title(p: &Path) -> String {
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

    fn to_menu(&mut self) {
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
        self.audio.stop();
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
        let mut song = Song { track: loaded.track, an: loaded.an, level: loaded.level, cursor: 0, last_t: start };
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
        self.audio.play(song.track.clone(), start as f64);
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
                let mut t = self.audio.song_time() as f32;
                if t < song.last_t {
                    t = song.last_t;
                }
                // Never let a hitch fast-forward the simulation by more than a moment.
                t = t.min(song.last_t + 0.25);
                song.last_t = t;
                self.world.t = t;
                while song.cursor < song.level.events.len() && song.level.events[song.cursor].at() <= t {
                    let e = song.level.events[song.cursor];
                    song.cursor += 1;
                    self.world.apply(&e);
                }
                let silent = song.an.in_silence(t) && song.an.in_silence(t + 0.7);
                let feat = feat_at(&song.an, t);
                let intensity = song.an.intensity_at(t);
                let bp = beat_pos(&song.level.beats, t);
                self.world.update(dt, t, input, bp, &feat, intensity, !silent);
                let dur = song.level.duration.max(0.1);
                hud.title = song.track.name.clone();
                hud.progress = t / dur;
                hud.time_left = dur - t;
                hud.bpm = song.level.bpm;
                hud.show_combat = true;
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
                        self.world.spawn_orbs(30.0);
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
                hud.banner_a = (it * 3.0).min(1.0) * ((3.2 - it) * 2.0).clamp(0.0, 1.0);
                let ready = self.ready.as_ref().map(|r| r.0 == self.qi).unwrap_or(false);
                hud.status = if !self.world.orbs.is_empty() {
                    "FLY INTO AN UPGRADE".into()
                } else if !ready {
                    "READING THE MUSIC".into()
                } else {
                    String::new()
                };
                if ready && ((it > 3.0 && self.world.orbs.is_empty()) || it > 12.0) {
                    self.world.orbs.clear();
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
        let mut fresh: Vec<LevelEvent> = Vec::new();
        for &(ft, f, raw) in &live.out.feats {
            live.gen.feed(ft, f.rms, &mut fresh);
            live.feats.push_back((ft, f, raw));
        }
        for o in &live.out.onsets {
            live.gen.on_onset(o, &mut fresh);
            if o.band == Band::Low && o.strength > 0.3 && o.t - live.last_beat_t > 0.28 {
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
        self.world.update(dt, clock, input, clock * 2.0, &feat, intensity, !idle_now);

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
fn menu_list(items: &[String], sel: &mut usize, x: f32, y: f32, u: f32, pal: &render::Palette) -> Option<usize> {
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
        let on = *sel == i;
        let bg = if on { Color::new(pal.secondary.r, pal.secondary.g, pal.secondary.b, 0.28) } else { Color::new(0.0, 0.0, 0.05, 0.5) };
        draw_rectangle(x, by, w, h, bg);
        draw_rectangle(x, by, 5.0 * u, h, if on { pal.accent } else { Color::new(1.0, 1.0, 1.0, 0.25) });
        if on {
            draw_rectangle_lines(x, by, w, h, 2.0 * u, pal.secondary);
        }
        text_left(it, x + 22.0 * u, by + h * 0.68, 28.0 * u, if on { WHITE } else { Color::new(1.0, 1.0, 1.0, 0.75) });
        if hover && is_mouse_button_pressed(MouseButton::Left) {
            *sel = i;
            activated = Some(i);
        }
    }
    if is_key_pressed(KeyCode::Enter) || is_key_pressed(KeyCode::KpEnter) {
        activated = Some(*sel);
    }
    activated
}

fn dim(a: f32) {
    draw_rectangle(0.0, 0.0, screen_width(), screen_height(), Color::new(0.02, 0.0, 0.06, a));
}

fn title_block(u: f32, pal: &render::Palette, time: f32) {
    let x = 110.0 * u;
    let wob = (time * 1.2).sin() * 3.0 * u;
    text_left(&spaced("HEARTLINE"), x + 4.0 * u, 214.0 * u + wob, 92.0 * u, Color::new(pal.secondary.r, pal.secondary.g, pal.secondary.b, 0.8));
    text_left(&spaced("HEARTLINE"), x, 210.0 * u + wob, 92.0 * u, WHITE);
    text_left("your music is the level", x + 4.0 * u, 250.0 * u, 28.0 * u, pal.primary);
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
        autofire: true,
        fullscreen: false,
        menu_sel: 0,
        dialog: None,
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
    app.audio.set_volume(if args.mute { 0.0 } else { app.volume });
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
            if !p.is_empty() {
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
                if is_key_pressed(KeyCode::Escape) && app.screen == Screen::Play {
                    if app.live.is_some() {
                        app.to_menu();
                    } else {
                        app.audio.set_paused(true);
                        app.screen = Screen::Paused;
                        app.menu_sel = 0;
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
                // Menu-like screens: drift mode, the player can still fly around.
                let mut input = app.gather_input(false);
                input.dash = false;
                let t = app.world.t;
                let ib = app.rend.time * 100.0 / 60.0;
                app.world.update(dt, t, &input, ib, &Feat::default(), 0.0, false);
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
                title_block(u, &pal, app.rend.time);
                let items = vec![
                    "PLAY FILES...".to_string(),
                    "PLAY FOLDER...".to_string(),
                    "LIVE AUDIO  (what your PC is playing)".to_string(),
                    format!("DIFFICULTY   {}", DIFFS[app.diff].1),
                    format!("VOLUME   {:.0}%", app.volume * 100.0),
                    format!("FULLSCREEN   {}", if app.fullscreen { "ON" } else { "OFF" }),
                    "QUIT".to_string(),
                ];
                let act = if app.dialog.is_none() { menu_list(&items, &mut app.menu_sel, 110.0 * u, 320.0 * u, u, &pal) } else { None };
                // left/right adjusts the selected setting
                let dirk = is_key_pressed(KeyCode::Right) as i32 - is_key_pressed(KeyCode::Left) as i32;
                if dirk != 0 {
                    match app.menu_sel {
                        3 => app.diff = ((app.diff as i32 + dirk).rem_euclid(3)) as usize,
                        4 => {
                            app.volume = (app.volume + 0.1 * dirk as f32).clamp(0.0, 1.0);
                            app.audio.set_volume(app.volume);
                        }
                        _ => {}
                    }
                }
                match act {
                    Some(0) => {
                        let (tx, rx) = channel();
                        std::thread::spawn(move || {
                            let exts: Vec<&str> = decode::AUDIO_EXTS.to_vec();
                            let r = rfd::FileDialog::new().set_title("Heartline - choose songs").add_filter("Audio", &exts).pick_files();
                            let _ = tx.send(r.unwrap_or_default());
                        });
                        app.dialog = Some(rx);
                    }
                    Some(1) => {
                        let (tx, rx) = channel();
                        std::thread::spawn(move || {
                            let r = rfd::FileDialog::new().set_title("Heartline - choose a music folder").pick_folder();
                            let _ = tx.send(r.map(|p| vec![p]).unwrap_or_default());
                        });
                        app.dialog = Some(rx);
                    }
                    Some(2) => {
                        app.audio.refresh_devices();
                        app.screen = Screen::LiveSetup;
                        app.menu_sel = 0;
                    }
                    Some(3) => app.diff = (app.diff + 1) % 3,
                    Some(4) => {
                        app.volume = if app.volume >= 0.99 { 0.0 } else { (app.volume + 0.1).min(1.0) };
                        app.audio.set_volume(app.volume);
                    }
                    Some(5) => {
                        app.fullscreen = !app.fullscreen;
                        set_fullscreen(app.fullscreen);
                    }
                    Some(6) => break,
                    _ => {}
                }
                let hy = sh - 60.0 * u;
                text_shadow("drop audio files or folders on the window to play", 110.0 * u, hy, 24.0 * u, Color::new(1.0, 1.0, 1.0, 0.8), -1);
                text_shadow("WASD move   mouse aim   SPACE dash   F auto-fire   ESC pause   F11 fullscreen", 110.0 * u, hy + 28.0 * u, 20.0 * u, Color::new(1.0, 1.0, 1.0, 0.55), -1);
                if app.dialog.is_some() {
                    text_shadow("waiting for the file dialog...", sw * 0.5, sh * 0.5, 28.0 * u, pal.accent, 0);
                }
            }
            Screen::LiveSetup => {
                dim(0.66);
                let devs = app.audio.devices();
                let x = 110.0 * u;
                text_left(&spaced("LIVE AUDIO"), x, 130.0 * u, 64.0 * u, WHITE);
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
                text_center(&spaced("PAUSED"), sw * 0.5, sh * 0.3, 80.0 * u, WHITE);
                let items = vec!["RESUME".to_string(), "RESTART RUN".to_string(), "QUIT TO MENU".to_string()];
                let act = menu_list(&items, &mut app.menu_sel, sw * 0.5 - 260.0 * u, sh * 0.42, u, &pal);
                if is_key_pressed(KeyCode::Escape) || act == Some(0) {
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
                text_center(&spaced(head), sw * 0.5, sh * 0.2, 84.0 * u, if app.died { pal.danger } else { pal.accent });
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
    app.audio.stop();
    app.audio.stop_live();
}
