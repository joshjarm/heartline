//! Developer tooling: headless CLI modes (`--analyze`, `--gen-test`, `--live-probe`) and the
//! self-test bot that plays the game so screenshots show real gameplay.

use std::f32::consts::TAU;
use std::path::Path;
use std::sync::atomic::AtomicU32;

use macroquad::math::{vec2, Vec2};

use crate::analysis::{Band, StreamAnalyzer, StreamOut};
use crate::game::{tile_center, tile_of, Input, World, HEART_R};
use crate::level::{Ev, COLS, HALF_H, HALF_W, ROWS};
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

pub fn bot_input(w: &World) -> Input {
    let p = w.player.pos;
    let mut force = Vec2::ZERO;

    // Hold a comfortable orbit around the Heart.
    let d = p.length().max(1.0);
    let want = 300.0;
    force += p / d * (want - d) * 0.012;
    // drift tangentially so the bot keeps moving
    force += vec2(-p.y, p.x) / d * 0.35;

    for b in &w.bullets {
        let to = p - b.pos;
        let dist = to.length().max(1.0);
        if dist < 170.0 {
            // weigh bullets that are heading at us
            let closing = b.vel.normalize_or_zero().dot(to / dist).max(0.0);
            force += to / dist * (170.0 - dist) / 170.0 * (1.2 + 2.5 * closing);
        }
    }
    for e in &w.enemies {
        let to = p - e.pos;
        let dist = to.length().max(1.0);
        if dist < 190.0 {
            force += to / dist * (190.0 - dist) / 190.0 * 2.0;
        }
    }
    // Armed tile under us: head for the nearest safe tile.
    if let Some((c, r)) = tile_of(p) {
        let tile = &w.tiles[r * COLS + c];
        if tile.armed && tile.strike_t - w.t < 0.8 {
            let mut best: Option<(f32, Vec2)> = None;
            for rr in 0..ROWS {
                for cc in 0..COLS {
                    if !w.tiles[rr * COLS + cc].armed {
                        let tc = tile_center(cc, rr);
                        if tc.length() < HEART_R + 70.0 {
                            continue;
                        }
                        let dd = tc.distance(p);
                        if best.map(|b| dd < b.0).unwrap_or(true) {
                            best = Some((dd, tc));
                        }
                    }
                }
            }
            if let Some((_, tc)) = best {
                force += (tc - p).normalize_or_zero() * 4.0;
            }
        }
    }
    for l in &w.lasers {
        let pos = if w.t < l.t0 { l.from } else { l.pos_at(w.t) };
        let delta = if l.horizontal { p.y - pos } else { p.x - pos };
        if delta.abs() < 120.0 && w.t > l.t0 - 0.6 {
            let push = delta.signum() * (120.0 - delta.abs()) / 120.0 * 3.0;
            if l.horizontal {
                force.y += push;
            } else {
                force.x += push;
            }
        }
    }
    // Pickups pull gently.
    let mut nearest: Option<(f32, Vec2)> = None;
    for s in &w.shards {
        let dd = s.pos.distance(p);
        if dd < 420.0 && nearest.map(|n| dd < n.0).unwrap_or(true) {
            nearest = Some((dd, s.pos));
        }
    }
    for o in &w.orbs {
        let dd = o.pos.distance(p);
        if nearest.map(|n| dd < n.0 + 200.0).unwrap_or(true) {
            nearest = Some((dd, o.pos));
        }
    }
    if let Some((_, tp)) = nearest {
        force += (tp - p).normalize_or_zero() * 0.9;
    }
    // Keep off the walls.
    force.x -= (p.x / HALF_W).powi(5) * 2.0;
    force.y -= (p.y / HALF_H).powi(5) * 2.0;

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
    let danger = w.bullets.iter().any(|b| b.pos.distance(p) < 26.0);
    Input {
        mv: if force.length() > 0.15 { force.normalize() } else { Vec2::ZERO },
        aim,
        fire: true,
        dash: danger && w.player.dash_cd <= 0.0,
    }
}
