# Heartline

> **Your music is the level.**

Drop in a song and fight it. Heartline listens to your music (kicks, snares, hi-hats, sustained
notes, bars, build-ups and drops) and turns it into a seeded, repeatable arena fight against
**the Heart**, a neon wireframe construct that pulses, aims and fires in time with the track.
Every attack is announced on the floor before it lands on the beat, so the fight is a dance you
can read, not a wall of bullets. The same song always gives the same fight.

Written in Rust (macroquad, cpal, symphonia, rustfft). Windows, no installer, no extra DLLs.

> **Project status.** This public repository is a snapshot of **Heartline 0.3.0**. The game is
> still in active development, but day-to-day work now happens in a private repository, so
> this one only gets occasional updates and may trail the latest build.

## Highlights

- **Any song becomes a level.** mp3, flac, ogg, wav, m4a and aac are analysed in-process
  (band energies, onsets, tempo, beats, bars, sections, silences). No external tools, no cloud.
- **The Heart plays the song.** Kicks make it fire announced patterns (cones, walls with a
  gap, pincers, crosses), snares call in enemies, hi-hats lay shard trails, sustained notes
  sweep lasers across the arena, bar lines slam down tile strikes, and drops send a
  shockwave and an elite.
- **Fair by design.** Difficulty is per second, not per song, so a slow track gets fewer,
  bigger moves and a fast one more, smaller ones. Every song starts gently, ramps up, and backs
  off for a few seconds when you take a hit.
- **Seven difficulty tiers** on a heart-rate theme, from the no-fail LULLABY to the one-life
  FLATLINE.
- **Roguelike runs.** Chain a song plus four more with SHUFFLE RUN: HP, upgrades and score
  carry over and the Heart gets tougher each track. Ten upgrades (TEMPO, CHORD, GAIN, PIERCE,
  SUSTAIN, GLIDE, PHASE, MAGNET, ECHO, ENCORE).
- **Live audio mode.** Play against whatever your PC is playing right now, reactive or with a
  delayed look-ahead through a second output device.
- **LAN Rally.** A beat-locked, two-PC paddle duel on the host's song.
- **Vaporwave look and sound.** Neon line art with depth, a synthwave sunset menu that reacts to
  the music, and three built-in synthwave tracks so it works before you point it at your library.
- **Accessible.** A VISUALS setting (FULL / SUBTLE / OFF) tones down pulsing and flashing.

## Play

Download `heartline.exe` from the [latest release](https://github.com/joshjarm/heartline/releases/latest) (or build it yourself,
see [Build](#build)) and run it. The menu plays music straight away: your **music library**
folder (shuffled) or, until one is set, three built-in synthwave tracks. `Q` / `E` change the
track.

- **PLAY THIS TRACK** — fight the song the menu is playing (it is already analysed, so it
  starts instantly). **SHUFFLE RUN** — that track plus four more as one roguelike run: HP,
  upgrades and score carry over and the Heart gets tougher each track.
- **PLAY FILES / PLAY FOLDER**, or drop audio files or folders on the window
  (mp3, flac, ogg, wav, m4a/aac ...).
- **MUSIC LIBRARY** — choose the folder the menu and SHUFFLE RUN draw from.
- **LIVE AUDIO** — play against whatever your PC is playing (reactive, or delayed look-ahead
  through a second output device; sound effects are off in live mode).
- **SYNC OFFSET** — if attacks land before you hear the beat (soundbars and Bluetooth add
  lag), raise it; if they land late, lower it.
- **VISUALS** — FULL / SUBTLE / OFF: how hard the scene pulses with the music (kick zoom
  punches, grid ripples, snare rings, the waveform ring around the Heart, build-up and drop
  hits). Choose SUBTLE or OFF if motion or flashing bothers you.

Settings are saved in `heartline.cfg` next to the exe.

| Input | Action |
|---|---|
| WASD / arrows | move |
| Mouse | aim |
| LMB (or auto-fire, toggle `F`) | fire — the gun fires on beat subdivisions |
| Space / Shift / RMB | dash (invulnerable while dashing) |
| Esc / P | pause (files) / Esc: back to menu (live) |
| Q / E (menu) | previous / next track |
| F11 | fullscreen |

How it plays:

- **Attacks are aimed and announced.** Before the Heart fires, a ring closes onto it and the
  danger zone is drawn on the floor (a cone, a wall with a gap, a pincer with an open lane, a
  cross). It fires exactly on the beat. Step out of the zone; the rest of the arena is yours.
- **Bars have a shape.** Each kind of section (calm / groove / surge / drop) has its own
  pattern per beat of the bar, and it comes back when that part of the song comes back.
- **Difficulty does not depend on the song.** Budgets are per second: a slow song gets fewer,
  bigger moves, a fast one more, smaller ones. Every song starts gently and ramps up; taking a
  hit backs the pressure off for a few seconds.

| You hear | You get |
|---|---|
| Kick / bass hit | the Heart fires its announced pattern |
| Snare / mid hit | an enemy spawns (its portal lands on the hit) |
| Hi-hats | shard trails to collect; dart streams in the hottest bars |
| Sustained notes | sweeping laser wall (starts on a beat) |
| Bar lines | tile-strike phrases: pillars rise for a bar, then slam beat by beat |
| A drop | shockwave ring with gaps + an elite |
| A calm part | upgrade orbs, heals |
| Silence / loading | the arena relaxes into the synthwave landscape until the music returns |

White square bullets can be shot down; hot round ones must be dodged. Shards fill the XP bar;
when it fills, three upgrade orbs appear — fly into one (every upgrade also heals 1). Empty the
Heart's bar before the song ends for a big bonus and a victory lap.

## Difficulty

Pick a tier in the menu (or with `--diff`). Only the attack budget changes between tiers, so a
song feels like the same song at every step.

| Tier | What you get |
|---|---|
| LULLABY | No-fail. Sparse attacks, slow bullets, long warnings. |
| RESTING | A gentle run: room to learn every pattern. |
| PULSE | The intended run. Denser patterns; lasers and tile strikes may overlap. |
| PALPITATION | Attacks come faster and enemies arrive in numbers. |
| FEVER | Big patterns on every beat. Little room to rest. |
| ARRHYTHMIA | Near-constant pressure and the widest patterns. |
| FLATLINE | One life. Everything at full force. |

## LAN Rally (two PCs)

A beat-locked paddle duel on the host's song. Both PCs must be on the same network.

- **Host:** menu → `LAN RALLY` → `HOST A MATCH`. The lobby shows your IP. Pick the song with
  Q / E (library) or O (any file), pick a character with LEFT / RIGHT, and press ENTER (ready).
  The song is then sent to the other PC (up to 200 MB).
- **Join:** menu → `LAN RALLY` → `JOIN A MATCH`. Hosts on your network are listed. Otherwise
  type the host's IP on the `TYPE AN IP` row and press ENTER. Pick a character and press ENTER.
- When both players are ready and the song is analysed on both PCs, a 3-second countdown starts
  the music on both at the same moment. WASD moves, the mouse aims, and click or SPACE swings.
- There is no pause: ESC asks whether to leave. If either PC leaves or the connection drops for
  3 seconds, the other returns to the menu with a message.
- **Windows Firewall** asks the first time you host (and sometimes when you join). Allow
  Heartline on **Private networks**. Heartline never changes firewall settings itself.
  It uses TCP port 47820 for the game and UDP port 47821 to find hosts.

## Command line

```
heartline.exe [files or folders...]      start a run with these songs
  --diff 0-6                             difficulty tier (0 = LULLABY, no-fail ... 6 = FLATLINE);
                                         names also work: chill, pulse, palpitation, fever,
                                         arrhythmia, flatline
  --seed N                               vary the generated level (default 0 = canonical)
  --live [--live-in NAME] [--live-out NAME]   start in live mode (device name substrings)
  --mute  --size WxH  --ss 1|2           misc (ss = supersampling)
Developer:
  --analyze FILE                         print the analysis + level summary (headless)
  --gen-test OUT.wav                     synthesize a structured test song
  --live-probe SECS                      list devices and test loopback capture (headless)
  --selftest DIR --shots a,b,c --secs N  bot plays, screenshots saved to DIR (add --wall for
                                         wall-clock shot times, --mortal to let the bot die)
  --lan-host FILE | --lan-join IP[:PORT] [--rally-bot]
                                         start a LAN Rally session; with --rally-bot the local
                                         player is the bot (auto character + ready). With
                                         --selftest DIR, state changes and the final score go to
                                         DIR\lan.log
  --sim FILES|FOLDER [--sample N] [--bot-delay MS] [--runs N]
                                         headless balance run: hits per minute, deaths, sources
```

## Build

Heartline targets Windows (live audio uses WASAPI loopback). You need the stable Rust toolchain
with the MSVC target; install it from <https://rustup.rs>.

```
cargo build --release
cargo test --release
```

The exe is `target\release\heartline.exe` (statically linked CRT, no extra DLLs). If it ever
exits silently, look for `heartline-crash.log` next to the exe.

`build.bat` does the same build for setups where Rust is installed per-user and `cargo` is not
on your `PATH`.

## Learn more

- [`DESIGN.md`](DESIGN.md): the design plan and the audio-to-level pipeline.
- [`docs/plans/`](docs/plans): plans for live audio, LAN Rally and 0.3.
- Heartline takes inspiration from Solus Heart (Risk of Rain 2), WindowKill and vaporwave /
  synthwave art. All code and art here are original and procedural.
