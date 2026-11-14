//! Developer tooling: headless CLI modes (`--analyze`, `--gen-test`, `--live-probe`) and the
//! self-test bot that plays the game so screenshots show real gameplay.

use std::f32::consts::TAU;
use std::path::Path;
use std::sync::atomic::AtomicU32;

use macroquad::math::{vec2, Vec2};

use crate::analysis::{Band, StreamAnalyzer, StreamOut};
use crate::game::{tile_of, Input, World, HEART_R};
use crate::level::{Ev, Pattern, COLS, FAN_STEP, HALF_H, HALF_W, PINCER_OFFSET, PINCER_SPREAD, WALL_GAP_HALF, WALL_HALF};
use crate::Args;

pub fn run_cli(args: &Args) {
    if let Some(out) = &args.gen_test {
        match gen_test_track(out) {
            Ok(()) => println!("wrote {}", out.display()),
            Err(e) => println!("failed: {e}"),
        }
    }
    if let Some(path) = &args.analyze {
        analyze_report(path, args);
    }
    if let Some(secs) = args.live_probe {
        live_probe(secs);
    }
    if args.sim {
        sim_report(args);
    }
}

// ---------------------------------------------------------------------------------------------
// Headless balance simulator
// ---------------------------------------------------------------------------------------------

/// What happened when the bot played one generated level.
pub struct SimOut {
    pub first_death: Option<f32>,
    pub deaths: u32,
    pub hit_times: Vec<f32>,
    /// (seconds spent, hits taken) per section kind: Calm, Normal, Intense, Drop.
    pub by_kind: [(f32, u32); 4],
    pub heart_broken: Option<f32>,
    pub level: u32,
    pub kills: u32,
    pub max_bullets: usize,
    pub mean_bullets: f32,
    pub max_enemies: usize,
    /// Hits by source: bullet, enemy, tile, laser, wave, heart.
    pub by_src: [u32; 6],
    pub by_pat: [u32; 12],
    pub by_tile: [u32; 9],
}

/// Bot input with a human-ish reaction delay: movement and dash decisions arrive `delay`
/// seconds late (aiming is immediate, a mouse is fast).
struct DelayedBot {
    q: std::collections::VecDeque<(f32, Input)>,
    cur: Input,
    delay: f32,
}

impl DelayedBot {
    fn new(delay: f32) -> Self {
        DelayedBot { q: std::collections::VecDeque::new(), cur: Input::default(), delay }
    }
    fn step(&mut self, w: &World, t: f32) -> Input {
        let raw = bot_input(w);
        self.q.push_back((t, raw));
        while let Some(&(qt, inp)) = self.q.front() {
            if qt <= t - self.delay {
                self.cur = inp;
                self.q.pop_front();
            } else {
                break;
            }
        }
        Input { mv: self.cur.mv, aim: raw.aim, fire: true, dash: self.cur.dash }
    }
}

/// Play `level` with the bot at a fixed 120 Hz step, no window, no audio. On death the bot is
/// revived so the whole song is measured.
pub fn simulate(an: &crate::analysis::Analysis, level: &crate::level::Level, diff: f32, bot_delay: f32, song_idx: u32) -> SimOut {
    use crate::analysis::SectionKind;
    let mut w = World::new(diff, false);
    w.begin_song(level.active_secs, song_idx, level.seed);
    w.beat_len = if level.bpm > 1.0 { 60.0 / level.bpm } else { 0.5 };
    let mut bot = DelayedBot::new(bot_delay);
    let dt = 1.0 / 120.0;
    let mut out = SimOut {
        first_death: None,
        deaths: 0,
        hit_times: Vec::new(),
        by_kind: [(0.0, 0); 4],
        heart_broken: None,
        level: 0,
        kills: 0,
        max_bullets: 0,
        mean_bullets: 0.0,
        max_enemies: 0,
        by_src: [0; 6],
        by_pat: [0; 12],
        by_tile: [0; 9],
    };
    let mut cursor = 0usize;
    let mut t = 0.0f32;
    let mut hits_seen = 0u32;
    let mut bullet_sum = 0.0f64;
    let mut steps = 0u64;
    while t < level.duration {
        w.t = t;
        while cursor < level.events.len() && level.events[cursor].at() <= t {
            let e = level.events[cursor];
            cursor += 1;
            w.apply(&e);
        }
        let silent = an.in_silence(t) && an.in_silence(t + 0.7);
        let feat = crate::feat_at(an, t);
        let input = bot.step(&w, t);
        w.sounds.clear();
        w.update(dt, t, &input, crate::beat_pos(&level.beats, t), &feat, an.intensity_at(t), !silent);
        if silent {
            w.calm_down();
        }
        let k = match w.section_kind {
            SectionKind::Calm => 0,
            SectionKind::Normal => 1,
            SectionKind::Intense => 2,
            SectionKind::Drop => 3,
        };
        if !silent {
            out.by_kind[k].0 += dt;
        }
        if w.stats.hits > hits_seen && std::env::var("HL_HITLOG").is_ok() {
            println!("   hit t={:.1} src={:?} pat={:?} bullet_age={:.2} r={:.0} pos=({:.0},{:.0}) pressure={:.2} pulses_pending={}", t, w.stats.by_src, w.stats.by_pat, w.last_hit_age, w.player.pos.length(), w.player.pos.x, w.player.pos.y, w.pressure(), w.pulses.len());
        }
        if w.stats.hits > hits_seen {
            out.by_kind[k].1 += w.stats.hits - hits_seen;
            for _ in hits_seen..w.stats.hits {
                out.hit_times.push(t);
            }
            hits_seen = w.stats.hits;
        }
        if w.dead {
            out.deaths += 1;
            out.first_death.get_or_insert(t);
            w.dead = false;
            w.player.hp = w.player.max_hp;
            w.player.invuln = 2.0;
        }
        if w.heart.broken && out.heart_broken.is_none() {
            out.heart_broken = Some(t);
        }
        out.max_bullets = out.max_bullets.max(w.bullets.len());
        out.max_enemies = out.max_enemies.max(w.enemies.len());
        bullet_sum += w.bullets.len() as f64;
        steps += 1;
        t += dt;
    }
    out.by_src = w.stats.by_src;
    out.by_pat = w.stats.by_pat;
    out.by_tile = w.stats.by_tile;
    out.level = w.player.level;
    out.kills = w.stats.kills;
    out.mean_bullets = (bullet_sum / steps.max(1) as f64) as f32;
    out
}

fn sim_report(args: &Args) {
    let mut files = args.files.clone();
    if args.sample > 0 && files.len() > args.sample {
        let n = files.len();
        files = (0..args.sample).map(|k| files[k * n / args.sample].clone()).collect();
    }
    if files.is_empty() {
        println!("--sim needs audio files or folders");
        return;
    }
    let delay = args.bot_delay / 1000.0;
    let diffs: Vec<usize> = if args.diff_set { vec![args.diff] } else { vec![0, 1, 2] };
    println!("bot reaction delay {:.0} ms, {} run(s) per difficulty; death = first death time, the bot is revived to finish the song", args.bot_delay, args.runs.max(1));
    println!("{:<26} {:>4} {:>4} {:<6} {:>5} {:>4} {:>4} {:>19} {:>17} {:>5} {:>3} {:>3} {:>2}  hits per 30 s", "track", "dur", "bpm", "diff", "death", "dths", "hits", "hits/min C/N/I/D", "src b/e/t/l/w/h", "heart", "lvl", "bul", "en");
    let mut src_tot = [[0u32; 6]; 3];
    let mut pat_tot = [[0u32; 12]; 3];
    let mut tile_tot = [[0u32; 9]; 3];
    let mut totals = [(0u32, 0u32, 0.0f32, 0.0f32); 3];
    for path in &files {
        let progress = AtomicU32::new(0);
        let stage = AtomicU32::new(0);
        let loaded = match crate::load_track(path, 0, 1.0, &progress, &stage) {
            Ok(l) => l,
            Err(e) => {
                println!("{:<34} error: {e}", path.file_stem().and_then(|s| s.to_str()).unwrap_or("?"));
                continue;
            }
        };
        let name: String = loaded.track.name.chars().take(25).collect();
        for &di in &diffs {
            let diff = crate::DIFFS[di].0;
            for run in 0..args.runs.max(1) {
                let level = crate::level::generate(&loaded.an, args.seed + run as u64, diff);
                let o = simulate(&loaded.an, &level, diff, delay, args.song_idx);
                let per_min = |k: usize| -> String {
                    let (secs, hits) = o.by_kind[k];
                    if secs < 5.0 { "  - ".to_string() } else { format!("{:4.1}", hits as f32 * 60.0 / secs) }
                };
                let mut buckets = vec![0u32; (level.duration / 30.0) as usize + 1];
                for &ht in &o.hit_times {
                    buckets[(ht / 30.0) as usize] += 1;
                }
                let bs: Vec<String> = buckets.iter().map(|b| b.to_string()).collect();
                for k in 0..6 {
                    src_tot[di][k] += o.by_src[k];
                }
                for k in 0..12 {
                    pat_tot[di][k] += o.by_pat[k];
                }
                for k in 0..9 {
                    tile_tot[di][k] += o.by_tile[k];
                }
                let srcs: Vec<String> = o.by_src.iter().map(|b| b.to_string()).collect();
                println!(
                    "{:<26} {:>4.0} {:>4.0} {:<6} {:>5} {:>4} {:>4} {:>19} {:>17} {:>5} {:>3} {:>3.0} {:>2}  {}",
                    name,
                    level.duration,
                    level.bpm,
                    ["chill", "normal", "hard"][di],
                    o.first_death.map(|d| format!("{d:.0}s")).unwrap_or_else(|| "-".into()),
                    o.deaths,
                    o.hit_times.len(),
                    format!("{} {} {} {}", per_min(0), per_min(1), per_min(2), per_min(3)),
                    srcs.join("/"),
                    o.heart_broken.map(|d| format!("{d:.0}s")).unwrap_or_else(|| "-".into()),
                    o.level,
                    o.mean_bullets,
                    o.max_enemies,
                    bs.join(" ")
                );
                let tt = &mut totals[di];
                tt.0 += 1;
                tt.1 += (o.deaths == 0) as u32;
                tt.2 += o.hit_times.len() as f32 * 60.0 / level.duration.max(1.0);
                tt.3 += o.first_death.unwrap_or(level.duration) / level.duration.max(1.0);
            }
        }
    }
    println!();
    for &di in &diffs {
        let (n, clears, hpm, frac) = totals[di];
        if n > 0 {
            println!(
                "{:<6} runs {:>3}  no-death clears {:>3} ({:>3.0}%)  mean hits/min {:.2}  mean survived fraction {:.2}  hits by src b/e/t/l/w/h {:?}",
                ["chill", "normal", "hard"][di],
                n,
                clears,
                clears as f32 * 100.0 / n as f32,
                hpm / n as f32,
                frac / n as f32,
                src_tot[di]
            );
            println!("       bullet hits by pattern ring/fan/spiral/cross/flower/arc/wall/burst/pincer/dart/orbiter/elite {:?}", pat_tot[di]);
            println!("       tile hits by checker/rows/cols/ring/cross/diag/scatter/colband/rowband {:?}", tile_tot[di]);
        }
    }
}

fn analyze_report(path: &Path, args: &Args) {
    let t0 = std::time::Instant::now();
    let progress = AtomicU32::new(0);
    let stage = AtomicU32::new(0);
    let diff = crate::DIFFS[args.diff].0;
    let loaded = match crate::load_track(path, args.seed, diff, &progress, &stage) {
        Ok(l) => l,
        Err(e) => {
            println!("error: {e}");
            return;
        }
    };
    let an = &loaded.an;
    println!("file      {}", path.display());
    println!("took      {:.2}s", t0.elapsed().as_secs_f32());
    println!("duration  {:.2}s  sr {}  frames {}", an.duration, loaded.track.sr, an.feats.len());
    println!("tempo     {:.2} BPM  beats {}  downbeat index {}", an.bpm, an.beats.len(), an.downbeat);
    let count = |b: Band| an.onsets.iter().filter(|o| o.band == b).count();
    println!("onsets    low {}  mid {}  high {}", count(Band::Low), count(Band::Mid), count(Band::High));
    println!("silences  {:?}", an.silences);
    println!("phrases   {}", an.phrases.len());
    println!("sections  {}", an.sections.len());
    for s in &an.sections {
        println!("   {:7.2} - {:7.2}  {:?}  intensity {:.2}", s.t0, s.t1, s.kind, s.intensity);
    }
    let first_beats: Vec<String> = an.beats.iter().take(10).map(|b| format!("{b:.3}")).collect();
    println!("beats[..10] {}", first_beats.join(" "));
    let lows: Vec<String> = an.onsets.iter().filter(|o| o.band == Band::Low).take(12).map(|o| format!("{:.3}", o.t)).collect();
    println!("low onsets[..12] {}", lows.join(" "));
    let mut c = [0usize; 10];
    let mut bullets = 0usize;
    for e in &loaded.level.events {
        let i = match e.ev {
            Ev::Beat { .. } => 0,
            Ev::Pulse { count, .. } => {
                bullets += count as usize;
                1
            }
            Ev::Tick { .. } => {
                bullets += 1;
                9
            }
            Ev::Spawn { .. } => 2,
            Ev::Shards { .. } => 3,
            Ev::Laser { .. } => 4,
            Ev::Tiles { .. } => 5,
            Ev::Shockwave { .. } => 6,
            Ev::Section { .. } => 7,
            Ev::Orbs => 8,
            _ => 9,
        };
        c[i] += 1;
    }
    println!(
        "level     seed {:016x}  events {}  pulses {} ({} bullets)  spawns {}  shards {}  lasers {}  tiles {}  shockwaves {}  orbs {}",
        loaded.level.seed,
        loaded.level.events.len(),
        c[1],
        bullets,
        c[2],
        c[3],
        c[4],
        c[5],
        c[6],
        c[8]
    );

    // Run the streaming analyser over the same audio to compare the live path.
    let mono = loaded.track.mono();
    let mut sa = StreamAnalyzer::new(loaded.track.sr);
    let mut out = StreamOut::default();
    for chunk in mono.chunks(480) {
        sa.push(chunk, &mut out);
    }
    let sc = |b: Band| out.onsets.iter().filter(|o| o.band == b).count();
    println!("stream    onsets low {}  mid {}  high {}  frames {}", sc(Band::Low), sc(Band::Mid), sc(Band::High), out.feats.len());
    let slows: Vec<String> = out.onsets.iter().filter(|o| o.band == Band::Low).take(12).map(|o| format!("{:.3}", o.t)).collect();
    println!("stream low onsets[..12] {}", slows.join(" "));
}

fn live_probe(secs: f32) {
    let audio = crate::engine::Audio::new();
    std::thread::sleep(std::time::Duration::from_millis(400));
    println!("output devices:");
    for d in audio.devices() {
        println!("   {}{}", d.name, if d.is_default { "  (default)" } else { "" });
    }
    audio.start_live(None, None, 0.0);
    let mut an: Option<StreamAnalyzer> = None;
    let mut out = StreamOut::default();
    let mut buf = Vec::new();
    let t0 = std::time::Instant::now();
    let mut total = 0usize;
    let mut peak = 0.0f32;
    let mut next_report = 1.0;
    while t0.elapsed().as_secs_f32() < secs {
        std::thread::sleep(std::time::Duration::from_millis(16));
        let info = audio.live_info();
        if !info.running {
            continue;
        }
        let a = an.get_or_insert_with(|| StreamAnalyzer::new(info.sr));
        buf.clear();
        audio.live_take(&mut buf);
        total += buf.len();
        for s in &buf {
            peak = peak.max(s.abs());
        }
        a.push(&buf, &mut out);
        if t0.elapsed().as_secs_f32() >= next_report {
            next_report += 1.0;
            println!(
                "t={:.1}s sr={} captured={} peak={:.4} onsets={} clock={:.2} since_packet={:.2}",
                t0.elapsed().as_secs_f32(),
                info.sr,
                total,
                peak,
                out.onsets.len(),
                audio.live_time(),
                info.since_packet
            );
        }
    }
    println!("status: '{}'", audio.status());
    audio.stop_live();
}

// ---------------------------------------------------------------------------------------------
// Test track synthesis
// ---------------------------------------------------------------------------------------------

/// Writes a ~64 s synthetic song (WAV) with an intro, groove, build, drop, breakdown, a silent
/// gap and an outro — enough structure to exercise every part of the analysis.
fn gen_test_track(out: &Path) -> Result<(), String> {
    let sr = 44_100usize;
    let bpm = 124.0f32;
    let beat = 60.0 / bpm;
    let total_secs = 64.0f32;
    let n = (total_secs * sr as f32) as usize;
    let mut l = vec![0.0f32; n];
    let mut r = vec![0.0f32; n];
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    let lead_in = 1.5f32;
    // (start_beat, end_beat, kick, snare, hats(0/8ths/16ths), bass, pad, lead)
    let parts: [(f32, f32, bool, bool, u8, bool, f32, bool); 7] = [
        (0.0, 16.0, false, false, 1, false, 0.5, false),  // intro (calm)
        (16.0, 48.0, true, true, 1, true, 0.3, true),     // groove
        (48.0, 56.0, false, true, 2, false, 0.5, false),  // build
        (56.0, 88.0, true, true, 2, true, 0.45, true),    // drop
        (88.0, 100.0, false, false, 0, false, 0.6, true), // breakdown
        (100.0, 108.0, false, false, 0, false, 0.0, false), // silence
        (108.0, 124.0, true, true, 1, true, 0.3, false),  // outro
    ];
    let add = |buf_l: &mut Vec<f32>, buf_r: &mut Vec<f32>, t0: f32, len: f32, pan: f32, f: &mut dyn FnMut(f32) -> f32| {
        let i0 = (t0 * sr as f32) as usize;
        let cnt = (len * sr as f32) as usize;
        for k in 0..cnt {
            let i = i0 + k;
            if i >= buf_l.len() {
                break;
            }
            let v = f(k as f32 / sr as f32);
            buf_l[i] += v * (1.0 - pan * 0.5);
            buf_r[i] += v * (1.0 + pan * 0.5);
        }
    };
    let bass_notes = [55.0f32, 55.0, 65.41, 49.0];
    let chords = [[220.0f32, 261.63, 329.63], [174.61, 220.0, 261.63], [196.0, 246.94, 293.66], [164.81, 207.65, 246.94]];
    for &(b0, b1, kick, snare, hats, bass, pad, lead) in parts.iter() {
        let mut b = b0;
        while b < b1 {
            let t = lead_in + b * beat;
            let bi = b as usize;
            let bar = bi / 4;
            if kick {
                add(&mut l, &mut r, t, 0.28, 0.0, &mut |x| {
                    let f = 45.0 + 85.0 * (-x * 28.0).exp();
                    (TAU * f * x).sin() * (-x * 11.0).exp() * 0.85
                });
            }
            if snare && bi % 2 == 1 {
                let mut ns: Vec<f32> = (0..(0.2 * sr as f32) as usize).map(|_| noise()).collect();
                for i in 1..ns.len() {
                    ns[i] = 0.6 * ns[i] + 0.4 * ns[i - 1];
                }
                add(&mut l, &mut r, t, 0.2, 0.1, &mut |x| {
                    let i = ((x * sr as f32) as usize).min(ns.len() - 1);
                    (ns[i] * 0.45 + (TAU * 190.0 * x).sin() * 0.3) * (-x * 22.0).exp()
                });
            }
            if hats > 0 {
                let div = if hats == 2 { 4 } else { 2 };
                for h in 0..div {
                    let th = t + h as f32 * beat / div as f32;
                    let mut prev = 0.0f32;
                    let amp = if h % 2 == 0 { 0.16 } else { 0.10 };
                    let ns: Vec<f32> = (0..(0.05 * sr as f32) as usize).map(|_| noise()).collect();
                    add(&mut l, &mut r, th, 0.05, -0.3, &mut |x| {
                        let i = ((x * sr as f32) as usize).min(ns.len() - 1);
                        let hp = ns[i] - prev;
                        prev = ns[i];
                        hp * amp * (-x * 90.0).exp()
                    });
                }
            }
            if bass {
                let f0 = bass_notes[bar % 4];
                for h in 0..2 {
                    let th = t + h as f32 * beat * 0.5;
                    add(&mut l, &mut r, th, beat * 0.45, 0.0, &mut |x| {
                        let s = (TAU * f0 * x).sin() + 0.4 * (TAU * 2.0 * f0 * x).sin();
                        s * 0.22 * (1.0 - (-x * 200.0).exp()) * (-x * 5.0).exp()
                    });
                }
            }
            if pad > 0.0 && bi % 4 == 0 {
                let ch = chords[bar % 4];
                add(&mut l, &mut r, t, beat * 4.0, 0.2, &mut |x| {
                    let env = (x * 3.0).min(1.0) * ((beat * 4.0 - x) * 3.0).clamp(0.0, 1.0);
                    let mut s = 0.0;
                    for (k, f) in ch.iter().enumerate() {
                        s += (TAU * f * x * (1.0 + 0.002 * k as f32)).sin();
                    }
                    s * 0.05 * pad * env
                });
            }
            if lead && bi % 8 == 2 {
                let f0 = [440.0f32, 523.25, 587.33, 392.0][(bar / 2) % 4];
                add(&mut l, &mut r, t, beat * 3.0, -0.1, &mut |x| {
                    let env = (x * 12.0).min(1.0) * ((beat * 3.0 - x) * 6.0).clamp(0.0, 1.0);
                    let vib = 1.0 + 0.004 * (TAU * 5.0 * x).sin();
                    ((TAU * f0 * vib * x).sin() + 0.35 * (TAU * 2.0 * f0 * vib * x).sin() + 0.15 * (TAU * 3.0 * f0 * x).sin()) * 0.16 * env
                });
            }
            b += 1.0;
        }
    }
    // Write 16-bit stereo WAV.
    let mut bytes: Vec<u8> = Vec::with_capacity(44 + n * 4);
    let data_len = (n * 4) as u32;
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&(sr as u32).to_le_bytes());
    bytes.extend_from_slice(&((sr * 4) as u32).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..n {
        for v in [l[i], r[i]] {
            let s = (v.clamp(-1.0, 1.0) * 30000.0) as i16;
            bytes.extend_from_slice(&s.to_le_bytes());
        }
    }
    std::fs::write(out, bytes).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Self-test bot
// ---------------------------------------------------------------------------------------------

/// Plays the game the way a person does: looks a short way ahead along each direction it could
/// move, scores what it would run into (bullets, enemies, armed tiles, lasers, shockwaves and
/// the Heart's announced danger zones) and takes the safest one.
pub fn bot_input(w: &World) -> Input {
    const SPEED: f32 = 430.0;
    const STEPS: [f32; 6] = [0.1, 0.22, 0.36, 0.52, 0.7, 0.9];
    let p = w.player.pos;
    let pi = std::f32::consts::PI;
    let wrap = |a: f32| -> f32 {
        let mut x = a.rem_euclid(TAU);
        if x > pi {
            x -= TAU;
        }
        x
    };

    // Where would we like to be if nothing threatened us? Near pickups, else a loose orbit.
    let mut goal: Option<Vec2> = None;
    let mut best_d = 460.0f32;
    for s in &w.shards {
        let dd = s.pos.distance(p);
        if dd < best_d {
            best_d = dd;
            goal = Some(s.pos);
        }
    }
    for o in &w.orbs {
        let dd = o.pos.distance(p) - 250.0;
        if dd < best_d {
            best_d = dd;
            goal = Some(o.pos);
        }
    }

    // In-flight danger of standing at `q` at time offset `tau`.
    let danger = |q: Vec2, tau: f32| -> f32 {
        let mut c = 0.0f32;
        let near = 1.0 / (0.25 + tau);
        for b in &w.bullets {
            if b.fade < 1.0 {
                continue;
            }
            let bp = b.pos + b.vel * tau;
            let d = bp.distance(q);
            let safe = b.r + 26.0;
            if d < safe {
                c += (1.0 + 3.0 * (safe - d) / safe) * near;
            }
        }
        for e in &w.enemies {
            let ep = e.pos + e.vel * tau * 0.8;
            let d = ep.distance(q);
            let safe = e.r + 46.0;
            if d < safe {
                c += (1.5 + 3.0 * (safe - d) / safe) * near;
            }
        }
        for pt in &w.portals {
            if pt.t1 - w.t < tau + 0.2 && pt.pos.distance(q) < 70.0 {
                c += 1.5;
            }
        }
        // The Heart itself.
        let dq = q.length();
        if dq < HEART_R + 60.0 {
            c += 3.0 * near;
        }
        // Tiles: being on a tile at the moment it slams.
        if let Some((tc, tr)) = tile_of(q) {
            let tile = &w.tiles[tr * COLS + tc];
            if tile.armed {
                let until = tile.strike_t - (w.t + tau);
                if until.abs() < 0.14 {
                    c += 8.0;
                } else if until > 0.0 && until < 0.5 {
                    c += 1.2 * (0.5 - until);
                }
            }
        }
        for l in &w.lasers {
            let lt = w.t + tau;
            if lt >= l.t0 - 0.05 && lt <= l.t0 + l.dur + 0.05 {
                let pos = l.pos_at(lt);
                let delta = if l.horizontal { q.y - pos } else { q.x - pos };
                if delta.abs() < 34.0 {
                    c += 8.0;
                } else if delta.abs() < 90.0 {
                    c += 0.6;
                }
            }
        }
        for wv in &w.waves {
            let lt = w.t + tau;
            if lt >= wv.t0 && (dq - wv.radius(lt)).abs() < 30.0 && !wv.in_gap(q.y.atan2(q.x)) {
                c += 8.0;
            }
        }
        // Announced pulses: do not be inside the zone when the bullets get here.
        let qang = q.y.atan2(q.x);
        for pp in &w.pulses {
            let arrive = (pp.t - w.t).max(0.0) + (dq - HEART_R).max(0.0) / pp.speed.max(120.0);
            if tau + 0.25 < arrive * 0.6 || pp.count == 0 {
                continue;
            }
            let off = wrap(qang - pp.aim);
            let inside = match pp.pattern {
                Pattern::Fan => off.abs() < FAN_STEP * (pp.count.max(1) as f32 - 1.0) * 0.5 + 0.1,
                Pattern::Burst => off.abs() < 0.12,
                Pattern::Pincer => (off.abs() - PINCER_OFFSET).abs() < PINCER_SPREAD * 0.5 + 0.08,
                Pattern::Wall => off.abs() < WALL_HALF + 0.06 && (off - pp.rot).abs() > WALL_GAP_HALF * 0.55,
                Pattern::Cross => (wrap(off * 4.0) / 4.0).abs() < 0.1,
                _ => false,
            };
            if inside {
                c += 1.6;
            }
        }
        c
    };

    let mut best_dir = Vec2::ZERO;
    let mut best_cost = f32::MAX;
    let cur = if w.player.vel.length() > 60.0 { w.player.vel.normalize() } else { Vec2::ZERO };
    for k in 0..17 {
        let dir = if k == 16 { Vec2::ZERO } else { vec2((k as f32 * TAU / 16.0).cos(), (k as f32 * TAU / 16.0).sin()) };
        let mut cost = 0.0f32;
        let mut q = p;
        for &tau in STEPS.iter() {
            q = p + dir * SPEED * tau;
            q.x = q.x.clamp(-HALF_W + 14.0, HALF_W - 14.0);
            q.y = q.y.clamp(-HALF_H + 14.0, HALF_H - 14.0);
            cost += danger(q, tau);
        }
        // Preferences (small next to any real danger).
        let dq = q.length();
        cost += ((dq - 320.0).abs() / 320.0) * 0.5;
        cost += ((q.x.abs() / HALF_W).powi(6) + (q.y.abs() / HALF_H).powi(6)) * 0.8;
        if let Some(g) = goal {
            cost += (q.distance(g) / 900.0) * 0.7;
        }
        cost -= dir.dot(cur) * 0.12; // keep going the way we are going
        if k == 16 {
            cost += 0.05;
        }
        if cost < best_cost {
            best_cost = cost;
            best_dir = dir;
        }
    }

    // Aim at the nearest enemy, else the Heart.
    let mut aim = Vec2::ZERO;
    let mut best = 520.0f32;
    for e in &w.enemies {
        let dd = e.pos.distance(p);
        if dd < best {
            best = dd;
            aim = e.pos;
        }
    }
    if w.heart.broken && best >= 520.0 {
        for b in &w.bullets {
            let dd = b.pos.distance(p);
            if b.soft && dd < best {
                best = dd;
                aim = b.pos;
            }
        }
    }
    // Dash when something is about to connect and walking will not fix it.
    let imminent = danger(p + best_dir * SPEED * 0.08, 0.08) > 6.0 || w.bullets.iter().any(|b| b.fade >= 1.0 && b.pos.distance(p) < 24.0);
    Input { mv: best_dir, aim, fire: true, dash: imminent && w.player.dash_cd <= 0.0 }
}
