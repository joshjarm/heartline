# Heartline — Design Plan

_A top-down vaporwave bullet-hell where the level **is** the song._

## 0. Revisions in 0.2 (after the first playtest, 2026-11-14)

These override the sections below where they differ:
- **Not a bullet hell.** Fewer, faster bullets; attacks are *directed* (cones, walls with a
  gap, pincers, crosses laid out around the player) and *announced* on the floor before they
  fire on the beat, so there is always obvious empty space to move into.
- **The song decides when and in what shape, never how hard.** All budgets are per second;
  tempo only picks the moments. Each song ramps up from a gentle start and backs off after a hit.
- **Bars have a shape:** one attack motif per section kind, reused when that kind returns.
- **Tile strikes are phrases** (sweeps, ripples, flips) that wind up for a bar and slam on beats.
- **Look:** neon line art with depth (2.5D), calm floor, music visualised as ridgelines at the
  arena rim and around the Heart; drift mode is the same world relaxing, not an overlay.
- **Sound:** the menu plays the library or built-in synthesised tracks and cross-fades into the
  level; a small bank of soft synthesised sound effects sits under the music.

## 1. Pitch

Drop in MP3s (or point the game at whatever your PC is playing). Heartline breaks the audio
down — bands, drum hits, tempo, beats, sections, silences — and turns it into a seeded,
repeatable arena fight against **the Heart**: a cube-cluster construct at the centre of a
tile-grid arena that pulses, fires and reshapes itself with the music. Survive the song
(the song length is the clock), break the Heart's integrity for bonus score, and carry
upgrades through a playlist like a roguelike run.

Inspirations (look and feel only — all code and art here are original and procedural):
- **Solus Heart (RoR2)** — a construct made of cubes over a tile arena, telegraphed tile
  strikes, sweeping laser rows, shoot-down-able projectiles, teal/white "virtual" palette.
- **WindowKill** — flat chunky geometric enemies on a dark field, twin-stick, minimal HUD.
  (Single window only. Nothing was decompiled or reused from the reference copy.)
- **Vaporwave / synthwave** — pink/cyan/purple palettes, striped sun, perspective grid,
  bloom, scanlines, chromatic aberration.

## 2. Audio → level pipeline

```
 file (mp3/flac/ogg/wav/m4a)            system audio (WASAPI loopback)
        │ symphonia decode                     │ cpal capture → 3 s delay line → speakers
        ▼                                      ▼
   STFT (2048 / hop 512, Hann)  ────────  same STFT, streaming
        │
        ├─ band energies (low 30-160 Hz · mid 300-3k · high 5k-16k) + 24-band spectrum
        ├─ spectral flux per band (log-compressed, max-filtered = "superflux-lite")
        ├─ onset picking per band (adaptive threshold + min gap)      → kick / snare / hat hits
        ├─ tempo (autocorrelation + prior) → beats (dynamic programming) → downbeats (bars)
        ├─ intensity curve (loudness + onset density, 0..1)
        ├─ sections (bar-level novelty) labelled Calm / Normal / Intense / Drop
        ├─ sustained mid-band phrases
        └─ silences
        ▼
   Level generator (seed = hash of the audio ⊕ run seed) → time-sorted event list
```

Everything is done in-process in Rust (rustfft); no external analyser is required. The same
band/onset code runs offline on a whole file and incrementally on the live buffer.

### Event mapping

| Music feature | Becomes | Type |
|---|---|---|
| Kick / low-band onset | Heart **pulse**: bullet ring / fan / spiral / cross / flower, size by hit strength × intensity | enemy fire |
| Snare / mid-band onset | **Enemy spawn** from the arena edge (portal telegraph lands on the hit) | enemies |
| Hat / high-band onset | **Shard trails** (calm) or **Darters** (intense) | reward / enemy |
| Sustained phrase | **Laser sweep** across the arena for the length of the phrase | obstacle |
| Downbeat (bar) | **Tile strike** pattern — tiles fill up like an approach circle and slam on the bar | obstacle |
| Section change | Palette shift, new pattern deck, Heart reform | structure |
| Drop | **Shockwave** ring with gaps + elite enemy | challenge |
| Calm section | **Upgrade orbs** + heal | reward |
| Beat grid | Your gun fires on beat subdivisions; drones fire on the beat | player rhythm |
| Spectrum | The arena floor is a live top-down spectrum analyser | visual |
| Silence / buffering / skip | **Drift mode** (see §4) | idle |

White ("soft") bullets can be shot down for shards; hot-coloured ones must be dodged.

## 3. Game modes

1. **Playlist run (main mode)** — pick files or a folder. Each song is fully analysed before it
   plays (best levels: global normalisation, beat grid, section plan, telegraphs that land on
   the hit). HP, upgrades and score carry across songs; the Heart gets tougher each track.
2. **Live mode** — capture what the PC is playing.
   - *Delayed (true look-ahead)*: route your music to a second/virtual output device; Heartline
     captures it, analyses ~3 s ahead and re-plays it to your speakers 3 s late, so spawns are
     telegraphed and land on the beat exactly like file mode.
   - *Reactive (zero setup)*: capture the default device with no delay; the Heart reacts the
     instant a hit is heard and spawns are telegraphed after the hit.
   Windows cannot delay a device's own output in place, hence the two-device routing for the
   delayed variant.
3. **Side mode (planned, not in 0.1)** — beat-runner/platformer using the same event stream.

Difficulty: Chill (no-fail, sparse) · Normal · Hard.

## 4. Drift mode (instead of loading screens)

Whenever there is no music to play against — analysing a file, live buffer filling, long
silence, a skipped song, between tracks — the arena dissolves into a synthwave scene: a
scrolling perspective grid and striped sun, with blocks, sine ribbons, polygons and line bursts
orbiting the player. You can still fly around. When the song starts or resumes, the orbiting
blocks fly out to become the arena tiles and the scene cross-fades back into the level.

## 5. Player kit

- WASD move · mouse aim · LMB fire (auto-fire toggle `F`) · Space/Shift/RMB dash (i-frames).
- Gun cadence is quantised to the beat (8ths → triplets → 16ths … via the TEMPO upgrade).
- Shards → XP → three upgrade orbs appear in the arena; fly into one to take it (no pause, so
  it works in live mode too). Upgrades: TEMPO, CHORD, GAIN, PIERCE, SUSTAIN, GLIDE, PHASE,
  MAGNET, ECHO (beat-firing drone), ENCORE (per-section shield).
- Combo multiplier from kills and pickups; hits halve it.

## 6. Tech

| Concern | Choice | Why |
|---|---|---|
| Language / engine | Rust + **macroquad** | Community guidance for small 2D games and straight ports; fast builds, shaders + render targets, single exe |
| Decode | **symphonia** | Pure Rust mp3/flac/ogg/wav/aac |
| DSP | **rustfft** + own onset/beat code | No C deps; identical offline and streaming paths |
| Audio I/O | **cpal** (WASAPI) | Sample-accurate playback clock; loopback capture of any output device |
| File dialog | **rfd** | Native picker; drag-and-drop also supported |
| Rendering | Scene → bloom (2 blur levels) → composite (aberration, scanlines, vignette) | Vaporwave glow |

Source layout: `rng.rs`, `decode.rs`, `analysis.rs` (offline + streaming), `engine.rs` (audio
thread: playback, loopback, delay line), `level.rs` (events + generators), `game.rs`
(simulation), `render.rs` (shaders, world, drift mode, HUD), `main.rs` (app flow, menu, CLI).

Determinism: level events are generated from a seeded PRNG keyed by the audio content, so the
same file always produces the same level (use `--seed N` to vary).

## 7. Milestones

- **0.1 (this build)** — pipeline, playlist run, the Heart + 5 enemy types + tiles/lasers/
  shockwaves, upgrades, drift mode, live mode (reactive + delayed), self-test tooling.
- **0.2** — online tempo tracking for live mode, settings persistence, more patterns/bosses,
  per-song high scores, gamepad.
- **0.3** — side mode (beat platformer), optional stem separation (e.g. Demucs) for
  drums/bass/vocals-specific mapping, level export/sharing by seed.
