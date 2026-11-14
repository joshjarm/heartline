# Heartline — Project State

_Last updated: 2026-11-13_

## Overview
Heartline is a Rust (macroquad) game that converts music into a top-down 2D vaporwave
bullet-hell / roguelike / timed-attack level. Songs (mp3 etc.) are analysed (bands, onsets,
tempo, beats, sections, silences) and turned into a seeded event list fought against "the
Heart" (a cube construct on a tile-grid arena). A live mode captures PC audio via WASAPI
loopback (reactive, or delayed ~3 s look-ahead through a second output device). Look:
Solus-Heart-adjacent cubes/tiles/lasers + WindowKill-style flat shapes, vaporwave palette.
Full plan in `DESIGN.md`, player-facing notes in `README.md`.

## Goals / Objectives
- [x] 0.1: playable playlist run from files, drift (idle) mode, live mode, test build in `#UserTesting/`
- [ ] Tune against real music after the user's first playtest (all tuning so far used a
      synthesized test track + a short live capture of whatever was playing)
- [ ] 0.2: online tempo tracking for live mode, settings persistence, more patterns/bosses,
      per-song high scores, gamepad
- [ ] 0.3: side mode (beat platformer), optional stem separation (e.g. Demucs)

## Current State
**0.1.0 works end to end** (2026-11-13). Verified by bot-driven self-tests with screenshots:
menu, live-setup screen, file run (drift → level fade, silence gap → drift → resume, tile
strikes, lasers, shockwave, enemies, upgrade orbs), two-track playlist with intermission,
results (clear + death), live reactive mode on real system audio, live delayed mode
(Soundbar → Razer headphones, muted). ~144 fps at 1280x720 with 2x supersampling (RTX 3080).
4 unit tests pass (tempo at 96/120/140 BPM, kick timing < 25 ms, streaming path, silence).

NOT verified by a human yet: game feel / difficulty balance, audio-visual sync by ear,
pause menu, file dialog + drag-and-drop (code paths exist, untested by the bot), real mp3s.

Build / run / test:
- `build.bat` (Rust is in `%USERPROFILE%\.cargo\bin`, NOT on PATH) → `target\release\heartline.exe`
- `cargo test --release` → analysis unit tests
- Self-test loop used during development (regenerate the test song first):
  ```
  heartline.exe --gen-test tmp\test.wav
  ffmpeg -y -i tmp\test.wav -b:a 192k tmp\test.mp3
  heartline.exe --analyze tmp\test.mp3
  heartline.exe tmp\test.mp3 --mute --selftest tmp\shots --shots 4,10.5,14 --secs 17 --size 1280x720
  heartline.exe --live --selftest tmp\live --wall --shots 5,9 --secs 10
  heartline.exe --live-out Razer --mute --selftest tmp\dl --wall --shots 1.5,8 --secs 10
  ```
  Screenshots land in the given folder with a `selftest.log`; read the PNGs to check visuals.
- Test build: `#UserTesting\Heartline-0.1.0-win-x64\` (exe + README + `demo-synth-track.mp3`).

## Next Steps
1. Get the user's playtest feedback on feel, difficulty and sync with real songs; tune
   `level.rs` (`bullet_rate`, spawn probabilities, tile cadence) and `analysis.rs` thresholds.
2. A/V offset setting (ms) in the menu if sync feels off on some output devices.
3. Online tempo/beat tracking for live mode so the gun cadence and bars follow the real beat
   (currently a fixed 120 BPM grid in live mode; kicks drive everything else).
4. Persist settings (difficulty, volume, devices) next to the exe.
5. More content: boss phases for the Heart, more enemy types/patterns, per-song high scores.

## Key Decisions & Notes
- 2026-11-13 — Engine: macroquad 0.4.16 (simple, fast builds, shaders/render targets). Audio:
  cpal 0.18 (WASAPI; building an *input* stream on an *output* device = loopback), symphonia 0.6
  (decode), rustfft 6 (analysis), rfd 0.18 (file dialog). No C dependencies.
- 2026-11-13 — Original code/art only. `references/Windowkill` (compiled Godot build) was used
  as a feel reference only; nothing was decompiled or copied. No reverse-engineering tools used.
- 2026-11-13 — Rust toolchain installed with rustup (`--no-modify-path`, minimal profile,
  stable 1.100, MSVC). Remove with `rustup self uninstall`.
- 2026-11-13 — Delayed live mode needs two output devices (Windows cannot delay a device's own
  output in place): capture device ≠ playback device, else it falls back to reactive.
- Level determinism: `level::generate` uses a splitmix64 PRNG seeded by a hash of the audio
  (⊕ `--seed`). Same file → same level. Live mode is seeded by the clock.
- Bullet density is bounded by a token bucket (`level::Budget`, `bullet_rate`): pulses that
  cannot be afforded still fire with `count = 0` so the Heart throbs on every kick.
- macroquad render-target cameras use positive zoom (y-down world, no flips needed when the
  target texture is drawn with `draw_texture_ex`). Scene is supersampled 2x (auto 1x on very
  large windows, `--ss` to force), then bloom (2 levels) + composite shader.
- cpal streams are created/owned on a dedicated audio thread (keeps COM apartment issues away
  from the window thread and the rfd file dialog, which runs on its own thread too).
- Loopback delivers no packets while nothing is playing: the delay line "rebuffers" (drift
  mode shows BUFFERING) and the reactive clock stalls → treated as silence.
- Release build uses `windows_subsystem = "windows"` (no console) and a static CRT
  (`.cargo/config.toml`). Panics are written to `heartline-crash.log` next to the exe.
- Source edits were made with Python patch scripts at times; files use CRLF line endings.

## Source map
`main.rs` app flow/menu/CLI · `analysis.rs` DSP (offline + `StreamAnalyzer`) · `level.rs`
events + `generate` + `LiveGen` · `game.rs` simulation · `render.rs` shaders/drawing/HUD ·
`engine.rs` audio thread (playback clock, loopback, delay line) · `decode.rs` symphonia ·
`tools.rs` CLI tools + self-test bot · `rng.rs` PRNG.

## Session Log
- 2026-11-13 — Researched stack, installed Rust, wrote DESIGN.md, built 0.1.0 (≈3.3k lines of
  Rust), tuned density/sections/palettes from self-test screenshots, verified file + live
  modes, packaged to `#UserTesting/Heartline-0.1.0-win-x64/`.
