# Heartline — Project State

_Last updated: 2026-11-14_

## Overview
Heartline is a Rust (macroquad) game that converts music into a top-down synthwave arena fight
against "the Heart". Songs (mp3 etc.) are analysed (bands, onsets, tempo, beats, bars, sections,
silences) and turned into a seeded event list. A live mode captures PC audio via WASAPI loopback.
Full plan in `DESIGN.md`, player-facing notes in `README.md`.

## Goals / Objectives
- [x] 0.1: playable playlist run from files, drift (idle) mode, live mode
- [ ] **0.2 (in progress, second playtest round):** directed/telegraphed attacks, song-independent
      difficulty, menu music, sound effects, visual overhaul (line art + depth), better beat sync
- [ ] Later: per-song high scores, gamepad, Heart boss phases, side mode (beat platformer)

## What the player said (2026-11-14 playtest of 0.1) — the brief for 0.2
- Dies within ~30 s on any song; difficulty should NOT depend on the song (slow songs were much
  easier). Only pacing / approach should change with the song.
- "Too much like a bullet hell": bullets everywhere with less and less pattern; attacks felt
  aimless. Wants attacks **directed**, leaving obvious empty space to move toward.
- Grid (tile) attacks are liked but need work. The background grid distracts from play.
- Music visualisation was a blocky equaliser; wants synthwave line art, shapes that morph, depth
  (not flat 2D). Drift/loading shapes looked terrible: must be part of the environment, not an
  overlay that fades in.
- Menu was silent: wants it to use their music (or standard music) and transition between songs
  like levels. Pause did not work. No sound effects (wants a careful approach to them).
- Typical music for this game: **`E:\Libraries\Music\Straftat`** (33 mp3s, mostly drum & bass).
  Whole library is `E:\Libraries\Music` (read-only: never copy/move/modify; skip `Senor Wooly Videos`).
- "REA" = "Reverse Engineer Anything" (GitHub); optional, not needed so far, nothing reverse-engineered.

## Current State (0.2.0 work, 2026-11-14)
Builds and runs; 12 unit tests pass (`cargo test --release`).

Done this session:
- **Level generator rewritten** (`level.rs`): all budgets are per second (bullets, enemy points,
  pulse / tile / laser intervals), so tempo no longer changes difficulty. Attacks are directed:
  `Pattern::{Fan, Burst, Wall, Pincer, Cross}` are laid out around the player's direction, locked
  when the pulse is announced (`PendingPulse.aim`), with a 0.42–0.6 s announce (`pulse_lead`).
  Each section KIND owns a motif (`Deck.moves`: pattern per half-beat of the bar), so a returning
  chorus brings its attack back. Tile strikes are phrases on the beat with a 1-bar wind-up
  (`TileSeq`: sweeps, ripples, flips, singles). Lasers snap to beats and never overlap tile
  wind-ups (except Hard). Hi-hat dart streams (`Ev::Tick`) only in hot sections.
- **Run-time difficulty** (`game.rs`): `World::pressure()` = ramp per song (`RAMP`) × run
  escalation × mercy after a hit; mechanics unlock at `P_*` thresholds. Gun fire rate and enemy
  fire cadence are normalised in seconds (`FIRE_INTERVALS`). 6 HP, every upgrade heals 1.
  Groove: bullets/enemies surge on the beat (`GROOVE_*`). Per-drum envelopes `World::hit`.
- **Analysis** (`analysis.rs`): metrical-level selection fixes 4:3 / 3:2 tempo
  errors (Straftat tracks were read as ~110 BPM, really ~84/168); sub-frame onset/beat times with
  bias correction (±1.5 ms on synthetic mixes); kick envelope ignores snares; downbeat uses chord
  change + backbeat; intensity is a per-song rank (mean ≈ 0.5) and section kinds are relative
  quotas; `beat_conf`, `downbeat_conf`, `StreamAnalyzer::live_beat()` (used by live mode).
- **Audio engine** (`engine.rs`): crossfade + looping (`play_ex`), `fade_out`, a sound-effect
  mixer with a synthesised bank (`Sfx`, 14 short soft sounds), soft clip on the mix bus.
- **Built-in music** (`synth.rs`): 3 original procedural synthwave tracks
  (`theme(i)`, ~1 s each to synthesise, loopable). They travel through playlists as pseudo
  paths `::theme0..2`.
- **Menu jukebox** (`main.rs`): the menu plays the music library (shuffled) or the built-in
  tracks, fully analysed so the scene reacts and PLAY starts instantly; Q/E change track;
  PLAY THIS TRACK / SHUFFLE RUN (5 tracks). Music cross-fades into the level. Between songs the
  next track rolls in after ~2 s (upgrade orbs stay on the field).
- **Settings** persist in `heartline.cfg` next to the exe (difficulty, volumes, sync offset,
  music library folder, auto-fire). SYNC OFFSET compensates laggy audio devices.
- **Pause fixed** (Esc was pausing and un-pausing in the same frame); P also pauses.
- **Tooling** (`tools.rs`): `--sim` headless balance simulator with a look-ahead bot and a
  reaction delay; `--drift` debug flag.

Balance numbers (bot with 120 ms reaction delay, 12 Straftat songs, `--sim`):
0.1 build: died at 28–54 s on every song (12 hits/min). Now: Chill 1.1 hits/min (100 % no-death
clears), Normal 1.6 (83 %), Hard 2.7 (50 %). With 0 ms delay the bot takes ~0.1 hits/min, so
every level is fair. Bullets on screen average 10–35 (was ~75).

- **Visual overhaul merged** (`render.rs`): 2.5D
  projection (`pj`), calm dark floor with thin grid, spectrum as line ridges around the arena rim
  and a crown around the Heart, wireframe morphing Heart and enemies, tile strikes as rising
  pillars, laser walls, danger-zone telegraphs for announced pulses (`draw_pulse_zone`, mirrors
  `World::emit_pulse` via `pulse_dirs`: keep the two in sync), vector stroke font (`vtext`),
  song-overview bar, synthwave drift/menu scene (sun, ridgeline mountains, ECG "heartline"),
  line-art HUD and menu widgets. ~144 fps at 1280x720.
- **Test build shipped:** `#UserTesting\Heartline-0.2.0-T1-win-x64\` (exe, README, and a
  `heartline.cfg` that points the music library at `E:\Libraries\Music\Straftat`).

NOT verified by a human yet: everything above by ear/feel; sound effect levels; live mode with
the new generator; file dialog paths.

Build / run / test:
- `build.bat`, or `"%USERPROFILE%\.cargo\bin\cargo.exe" build --release` (Rust is NOT on PATH).
- Balance: `heartline.exe --sim "E:\Libraries\Music\Straftat" --sample 12 --bot-delay 120`
  (add `--diff normal`, `--runs N`, `--song-idx N`; `HL_HITLOG=1` prints every hit).
- Analysis: `heartline.exe --analyze FILE`.
- Screenshots: `heartline.exe FILE --mute --selftest tmp\s1 --shots 20,45 --secs 50 --size 1280x720`
  (`--start S`, `--seed N`, `--diff`, `--mortal`, `--drift`); menu: no file + `--wall --shots 5 --secs 6`.
- Test build goes to `#UserTesting\Heartline-<version>-win-x64\`.

## Next Steps
1. Wait for the player's verdict on `Heartline-0.2.0-T1` (shipped 2026-11-14).
2. Act on the next playtest: sound-effect taste/levels, pattern feel, tile phrases, difficulty.
3. Half/double tempo choice is still a coin flip on some songs (DnB reads at half time, ~84).
   Gameplay is time-normalised so it is not harmful, but bar-based events are half as frequent.
4. Live mode: sound effects are off (loopback would hear them); test with the new generator.
5. Ideas: Heart boss phases every third of its HP, calibration screen for SYNC OFFSET, per-song
   high scores, gamepad.

## Key Decisions & Notes
- 2026-11-13 — Engine: macroquad 0.4.16; audio cpal 0.18 (WASAPI loopback), symphonia 0.6,
  rustfft 6, rfd 0.18. No C dependencies. Original code/art only.
- 2026-11-14 — Parallel work: `analysis.rs`, `synth.rs` and `render.rs` were each developed on their own
  copy of the project and merged back one file at a time. The copies were deleted after merging.
- 2026-11-14 — Difficulty is tuned with `--sim`, not by feel: the delayed bot at 120–200 ms died
  at ~30 s on 0.1, matching the player's report, so it is the proxy for "average player".
- Line endings: `analysis.rs` and `synth.rs` are CRLF, everything else LF (do not normalise).
- `grep -c $'\r$'` lies in Git Bash here; check line endings with Python.
- Level determinism: `level::generate` uses a splitmix64 PRNG seeded by a hash of the audio
  (⊕ `--seed`). Difficulty ramp/mercy are applied at run time by `World`, not baked in.
- The gameplay plane (z = 0) must stay an exact top-down view: `View::to_world` is used for aiming.
- The Windows "Music" known folder on this PC is empty (the real library is `E:\Libraries\Music`),
  so the menu falls back to the built-in tracks until MUSIC LIBRARY is set.
- Rust toolchain: rustup, stable 1.100 MSVC, in `%USERPROFILE%\.cargo\bin` (not on PATH).
  `cmd /c .\build.bat` (plain `build.bat` fails: `NoDefaultCurrentDirectoryInExePath=1`).
- Release build is `windows_subsystem = "windows"` with a static CRT; panics are written to
  `heartline-crash.log` next to the exe. CLI modes print when piped (`| cat`).
- Delayed live mode needs two output devices. Output devices here: "Soundbar Speaker (Realtek(R)
  Audio)" (default) and "Razer Headphones (Razer Kraken V3)".
- Solus Heart (RoR2) and WindowKill are feel references only.

## Source map
`main.rs` app flow / menu / jukebox / settings / CLI · `analysis.rs` DSP (offline +
`StreamAnalyzer`) · `level.rs` events + `generate` + `LiveGen` · `game.rs` simulation ·
`render.rs` shaders / drawing / HUD / UI widgets · `engine.rs` audio thread (playback, crossfade,
SFX, loopback, delay line) · `synth.rs` built-in music · `decode.rs` symphonia · `tools.rs` CLI
tools, balance simulator, bot · `rng.rs` PRNG.

## Session Log
- 2026-11-13 — Researched stack, installed Rust, wrote DESIGN.md, built 0.1.0, packaged to
  `#UserTesting/Heartline-0.1.0-win-x64/`.
- 2026-11-14 — First playtest feedback. 0.2 work: generator rewrite (directed, per-second
  budgets, motifs, tile phrases), pressure ramp + mercy, balance simulator, analysis overhaul,
  built-in music, visual overhaul, menu jukebox, SFX, settings,
  pause fix. Shipped `Heartline-0.2.0-T1` to `#UserTesting`, removed the 0.1.0 build.
