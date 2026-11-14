# Heartline

_Your music is the level._ A top-down vaporwave bullet-hell where every song becomes a seeded,
repeatable arena fight against **the Heart** — a cube construct that pulses and fires with the
music. Written in Rust (macroquad + cpal + symphonia + rustfft). See `DESIGN.md` for the plan.

## Play

Run `heartline.exe`, then either:

- **Drop audio files or folders on the window**, or choose **PLAY FILES / PLAY FOLDER**
  (mp3, flac, ogg, wav, m4a/aac …). Several songs = one roguelike run: HP, upgrades and score
  carry over and the Heart gets tougher each track.
- **LIVE AUDIO** — play against whatever your PC is playing.
  - *Reactive*: pick the device your music plays on, leave playback on "nowhere". Zero setup.
  - *Delayed look-ahead*: send your music app to a second or virtual output device, listen to
    that device in Heartline, and play it back on your real speakers/headphones. You hear the
    music ~3 s late and every spawn is telegraphed to land on the beat.

| Input | Action |
|---|---|
| WASD / arrows | move |
| Mouse | aim |
| LMB (or auto-fire, toggle `F`) | fire — the gun fires on beat subdivisions |
| Space / Shift / RMB | dash (invulnerable while dashing) |
| Esc | pause (files) / back to menu (live) |
| F11 | fullscreen |

What the music does:

| You hear | You get |
|---|---|
| Kick / bass hit | the Heart fires a bullet pattern |
| Snare / mid hit | an enemy spawns (its portal lands on the hit) |
| Hi-hats | shard trails to collect, darters when it's intense |
| Sustained notes | sweeping laser |
| Each bar | tile strikes — tiles fill up, then slam on the downbeat |
| A drop | shockwave ring with gaps + an elite |
| A calm part | upgrade orbs, heals |
| Silence / loading / buffering | drift mode — fly around the synthwave scene until it fades back in |

White square bullets can be shot down; hot round ones must be dodged. Shards fill the XP bar;
when it fills, three upgrade orbs appear — fly into one. Empty the Heart's bar before the song
ends for a big bonus and a victory lap.

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
```

## Build

Rust (stable, MSVC) is installed in the user profile but is not on PATH — use `build.bat`, or:

```
"%USERPROFILE%\.cargo\bin\cargo.exe" build --release
"%USERPROFILE%\.cargo\bin\cargo.exe" test --release
```

The exe is `target\release\heartline.exe` (statically linked CRT, no extra DLLs).
If it ever exits silently, look for `heartline-crash.log` next to the exe.
