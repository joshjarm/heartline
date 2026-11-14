# Heartline

_Your music is the level._ A top-down vaporwave bullet-hell where every song becomes a seeded,
repeatable arena fight against **the Heart** — a wireframe construct that pulses and fires with the
music. Written in Rust (macroquad + cpal + symphonia + rustfft). See `DESIGN.md` for the plan.

## Play

Run `heartline.exe`. The menu plays music straight away: your **music library** folder
(shuffled) or, until one is set, three built-in synthwave tracks. `Q` / `E` change the track.

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

## Command line

```
heartline.exe [files or folders...]      start a run with these songs
  --diff chill|normal|hard               difficulty (chill = no-fail)
  --seed N                               vary the generated level (default 0 = canonical)
  --live [--live-in NAME] [--live-out NAME]   start in live mode (device name substrings)
  --mute  --size WxH  --ss 1|2           misc (ss = supersampling)
Developer:
  --analyze FILE                         print the analysis + level summary (headless)
  --gen-test OUT.wav                     synthesize a structured test song
  --live-probe SECS                      list devices and test loopback capture (headless)
  --selftest DIR --shots a,b,c --secs N  bot plays, screenshots saved to DIR (add --wall for
                                         wall-clock shot times, --mortal to let the bot die)
  --sim FILES|FOLDER [--sample N] [--bot-delay MS] [--runs N]
                                         headless balance run: hits per minute, deaths, sources
```

## Build

Rust (stable, MSVC) is installed in the user profile but is not on PATH — use `build.bat`, or:

```
"%USERPROFILE%\.cargo\bin\cargo.exe" build --release
"%USERPROFILE%\.cargo\bin\cargo.exe" test --release
```

The exe is `target\release\heartline.exe` (statically linked CRT, no extra DLLs).
If it ever exits silently, look for `heartline-crash.log` next to the exe.
