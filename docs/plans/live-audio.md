# Plan: Live Audio rework (analysed look-ahead, per-app capture, long delays)

_Written by the planner agent, 2026-11-13. Status: not started. Open questions in §6 need the user's answers before steps 5, 7 and 8._

## 1. Goal
Make live mode produce levels as good as file mode: a real beat grid, bars, Calm/Normal/Intense/Drop sections, drops and phrase lasers. To get there, buffer 12-180 s of captured audio, run the offline analysis pipeline on rolling windows, and play the audio back delayed. The player can capture one specific program (Windows process loopback), and the zero-delay reactive mode stays available.

## 2. Approach
**Core idea: split the offline pipeline into two parts, then run the second part on windows.**
- **Front end (incremental, bit-identical to offline).** `Spectral::process` already runs one frame at a time. Live mode will run it continuously on a worker thread and append raw per-frame features (rms, flux[3], energy[3], spec[24], about 124 B per frame, about 11.6 KB/s) to a `FrameStore`. Frame `i` is centred at `i*hop`, the same as `analyze`. So windows re-read cheap frame data, not raw samples or FFTs. The analysis side never needs a large mono sample buffer.
- **Back end (windowed).** This covers onset picking, tempo, beats, downbeats, intensity, sections, phrases and silences. It runs on a window of the last `WINDOW_SECS` (about 45 s) every `STRIDE` (2 s of new audio). It only commits results in the region `[committed_until, head - GUARD)`, so edge effects (smoothing, novelty needs 3 bars ahead, phrase ends up to 4 s) never reach committed data. Continuity across windows comes from an explicit carry state:
  - **Tempo:** a Gaussian prior on the previous period, and octave lock (a 2x or 0.5x jump keeps the old octave).
  - **Beats:** the DP beat tracker is **anchored** on the last committed beat (the DP starts at that frame), so the beat chain never gaps or doubles at a window seam.
  - **Bars:** an absolute beat index plus decayed per-phase downbeat scores, with hysteresis (a phase change needs 2 windows in a row and only happens on a bar).
  - **Normalisation:** refs (percentiles for rms, energy, spec, loudness p10/p95, onset density) come from a capped history (180 s) and are EMA-smoothed between windows.
  - **Sections:** bar-feature z-scores use the bar history. A section stays open across windows. Its kind is decided when its boundary commits, from at least `GUARD` seconds (≥4 bars) of known future audio, which is close to the offline rule. Over-long sections are force-split at 26 s (hard limit 32 s), snapped to a bar. The Drop rule is the same as offline.
  - **Phrases and silences:** open phrases and open silences are carried across windows.
- **Generator.** `level::generate` becomes a stateful `LevelGen::extend(chunk)` with **per-feature RNG streams** (separate streams for sections, onsets, lasers and tiles). That way, feeding a whole song as one chunk or as many chunks gives identical events. File mode becomes `extend(whole) + finish()`.
- **Scheduling.** Event `t` is on the capture timeline. The engine's delayed `live_time()` already returns capture-timeline seconds of what is audible now. The look-ahead budget is `delay - GUARD - STRIDE - compute`, which must be at least the longest telegraph (1.3 s) plus 1 s. That is why the minimum delay is 12 s.
- **Continuous timeline.** Loopback sends no packets during silence. The engine will insert silence frames by wall clock (and honour WASAPI's silent-buffer flag) so the timeline never shrinks. A pause in the source app then turns into an ordinary silence `delay` seconds later, and the game handles it the way file mode handles a silent gap: drift mode, then resume.

**Per-app capture: use the `wasapi` crate** (HEnquist, pure Rust, built on `windows` 0.62).
- It already wraps the hard part, `ActivateAudioInterfaceAsync` with a COM completion handler, as `AudioClient::new_application_loopback_client(pid, include_tree)`. It also exposes session enumeration (`DeviceEnumerator` → `get_device_collection` → `get_iaudiosessionmanager` → `get_audiosessionenumerator` → `get_session(i)` → `get_process_id`, `get_state`, `get_audiometerinformation().get_peak_value()`).
- Using the raw `windows` crate instead means about 300 lines of unsafe COM, including writing our own `IActivateAudioInterfaceCompletionHandler` and `IAgileObject`. That's not worth it.
- Use `windows` directly only for small helpers: process exe name via `QueryFullProcessImageNameW`, and parent PID via ToolHelp. Use the same version wasapi pulls in.
- Pin wasapi to an exact version, because it breaks its API often. The current release is 0.27.0; its MSRV is 1.85, edition 2024, and our toolchain is 1.100.

**Alternative considered: record the whole track, then play it.** This gives exact file-mode quality for very little code (capture, then `analyze`/`generate`, then the existing file-playback `Track` path), so it is offered as a third mode. It doesn't replace look-ahead, because it means listening to every song once before you can play it.

## 3. Steps
Dependencies: 1 and 2 can run in parallel. Step 3 needs 1 and 2. Steps 4 and 6 are independent of 1-3 (do 4 before 6, because both touch `engine.rs`). Step 5 needs 3 and 4. Step 7 needs 5 and 6. Step 8 needs 4. Step 9 is optional.

**1. Front-end split, no behaviour change** (audio-dsp-engineer, parallel with 2)
- In `src/analysis.rs`, add `pub struct FrameStore { hop_dt, base_frame: u64, rms, flux:[Vec;3], energy:[Vec;3], spec }` with `trim_before(frame)`.
- Add `pub struct FrontEnd { sp: Spectral, buf, next_frame }` with `new(sr)` and `push(&mut self, mono:&[f32], out:&mut FrameStore)`. It pre-pads n/2 zeros so frame i is centred at i*hop, the same as `analyze`.
- Split `analyze` into `front_end_all(mono, sr, progress) -> FrameStore` plus `analyze_frames(&FrameStore, duration, seed) -> Analysis`, and move the existing body there unchanged.
- Tests:
  - `frontend_chunked_matches_offline`: 480-sample and 4096-sample chunks both give bit-identical arrays.
  - `analyze_golden`: hash of beats, onsets and sections for `click_track(120)` and a 2-tempo track, recorded before the refactor and asserted after.

**2. Chunkable generator** (gameplay-programmer, parallel with 1)
- In `src/analysis.rs`, define `pub struct Chunk { t0, t1, hop_dt, feats: Vec<Feat>, intensity: Vec<f32>, onsets, beats: Vec<(f32 /*t*/, u64 /*abs idx*/, bool /*down*/)>, sections: Vec<Section> /*opened in [t0,t1), t1 may be f32::INFINITY while open*/, phrases, silences, bpm, boundary: bool }` and `Analysis::as_chunk(&self) -> Chunk`.
- In `src/level.rs`, add `pub struct LevelGen` holding everything `generate` keeps as locals now: palette, decks, `sec_rot`, `pulse_n`, `low_seen`, `last_*`, `Budget`, `tile_n`, the known-bars list and a sections-so-far vector. Add `new(seed, diff)`, `extend(&mut self, c:&Chunk, out:&mut Vec<LevelEvent>)` and `finish(duration, out)`.
- Use separate `Rng` streams: `seed ^ K_SECTIONS/K_ONSETS/K_LASERS/K_TILES/K_GRID`.
- Make the "section with no kicks → grid pulses" rule chunk-local: per bar, if the previous 2 bars had fewer than 2 low onsets, pulse on that bar's beats.
- `generate()` becomes a wrapper: `extend(as_chunk)`, then `finish`, then sort.
- Test `chunked_equals_whole`: split the test-track `Analysis` at 7 arbitrary times and assert the event lists are equal.
- Note: file-mode levels change once (new RNG layout). Accept that and record it in PROJECT_STATE.

**3. Rolling analyser + `--live-sim`** (audio-dsp-engineer)
- In `src/analysis.rs`:
  - Add `pub struct RollingAnalyzer { fe: FrontEnd, store: FrameStore, carry: Carry, cfg: RollCfg, committed_until: f32, frames_seen: u64 }` with `push(&mut self, mono, out:&mut Vec<Chunk>)`. Windows fire on **sample-count multiples of STRIDE** only, so the output is the same whatever the push size.
  - Factor the stages of `analyze_frames` into reusable fns: `onsets_in`, `comb_envelope`, `estimate_tempo` (new param `prior: Option<f32>`), `track_beats` (new param `anchor: Option<usize>`), `downbeat_scores`, `intensity_curve`, `bar_features`, `novelty_bounds`, `phrases_in`, `silences_in`. Offline calls them with `None` priors on the full range; rolling calls them with the carry.
  - `Carry` holds: period, last committed beat frame and abs index, phase scores[4], refs (EMA), bar-feature history (last 64 bars), open section {t0, kind, sum, n}, open phrase, open silence.
  - Song boundary (resets tempo prior, phase and section history; sets `chunk.boundary`): a silence ≥ `BOUNDARY_SILENCE`, or a tempo jump > 8% that persists for 2 windows together with a novelty peak.
  - First commit needs at least 10 s captured.
- In `src/tools.rs`, add `--live-sim <file> [--delay N]`. It decodes the file, feeds 10 ms chunks into `RollingAnalyzer` and `LevelGen`, and prints:
  - BPM per window
  - beat agreement with offline (% of offline beats with a live beat within 70 ms)
  - section boundaries matched within ±1 bar, and kinds side by side
  - event counts by type, compared with `--analyze`
  - window compute ms (p50 and max)
  - event-list hash
- Add `--live-sim-gap A,B` to zero out a region (simulates a pause or skip).
- Unit tests:
  - `rolling_no_seam_gaps`: on a click track, inter-beat intervals stay within ±5% everywhere, including window seams.
  - `rolling_tempo_switch`: a 96→140 BPM track locks to the new tempo within 2 windows.
  - `rolling_chunk_invariant`: 480- and 4096-sample pushes give the same hash.
  - `rolling_silence_boundary`.

**4. Engine: long ring, continuous timeline, file-as-source, pause** (audio-dsp-engineer)
- In `src/engine.rs`:
  - Extract `fn push_frames(lv:&mut LiveState, l_r: impl Iterator<Item=(f32,f32)>)` from `capture_stream` (step 6 reuses it).
  - Add a 10 ms tick on the audio thread (use `rx.recv_timeout`) that inserts zero frames for wall-clock gaps longer than 50 ms, so `write_abs` follows real time.
  - `build_capture_stream`: ring = `(delay + PAUSE_MARGIN) * sr`. Change `rebuffering` semantics: it is only true during the initial fill.
  - Add `Audio::set_live_paused(bool)`: holds `read_pos` and `live_time`. On resume, if `avail > cap - sr`, jump to the target delay and report the skip through `LiveInfo.skipped: f32`.
  - Add `Audio::tap() -> LiveTap` (a clonable `Send` handle on `Arc<Shared>` with `take(&mut Vec<f32>)` and `head_time()`), so the worker thread can drain capture.
  - Extend `LiveInfo` with `fill: f32` (0..1), `paused` and `skipped`.
  - Add `Cmd::StartLive{ source: Source, playback, delay }`, where `enum Source { Device(Option<DeviceId>), File(PathBuf, Vec<(f32,f32)> /*gaps*/), App{pid:u32, tree:bool} }`. `File` spawns a feeder thread that pushes decoded audio at real-time rate and skips the gap ranges, so selftests can play "live" without system audio. `App` stays unimplemented until step 6 and falls back to `Device(None)`.
- In `src/main.rs`, `parse_args` gets `--live-file <path>`, `--live-gap a,b` and `--delay N`.
- Keep the existing `LiveGen` flow and the 6 s menu clamp, so the game plays exactly as before.

**5. Live worker + analysed delayed mode in game** (gameplay-programmer)
- New `src/live.rs`:
  - `pub struct EventBatch { seq: u32, t0: f32, t1: f32, events: Vec<LevelEvent>, feats: Vec<(f32,Feat,f32 /*intensity*/)>, beats: Vec<(f32,bool)>, bpm: f32, boundary: bool }`.
  - `pub struct LiveWorker { rx: Receiver<EventBatch>, progress: Arc<LiveProgress /*atomics: analysed_until, bpm_x100, sections*/>, stop: Arc<AtomicBool> }` and `LiveWorker::spawn(tap, sr, cfg, user_seed, diff)`.
  - The thread named `heartline-live` loops: `tap.take`, then `RollingAnalyzer::push`, then `LevelGen::extend`, then send. It never touches cpal.
  - Seed = fnv1a of the first committed window's samples ⊕ user seed (not the clock).
  - It also publishes head-of-capture `Feat`s, which drive the drift visuals during the fill.
- In `src/main.rs`:
  - `LiveRun` becomes `enum LiveKind { Reactive{ an, gen: LiveGen, … existing fields }, Analysed{ worker, events: Vec<LevelEvent>, cursor, feats: VecDeque, beats: Vec<f32>, bpm } }`.
  - `start_live` picks Analysed when `delay >= MIN_ANALYSED_DELAY`.
  - Split `step_live` into `step_live_reactive` (the current body) and `step_live_analysed`. The analysed version:
    - drains batches with `try_recv` and appends events (each batch is already sorted by `at()`; merge-insert only the tail)
    - fires events with a cursor against `live_time()`, the same as `step_files`
    - uses `feat_at` and `beat_pos` on the received data, so the gun cadence follows the real beat instead of `clock*2.0`
    - goes idle when the current frame is a silence
    - on `boundary`, runs the `world.calm_down()` / `begin_song()` reform with a "NEXT TRACK" banner
    - during the fill: drift mode, `hud.status = "LISTENING  23 / 60 s  -  124 BPM"`, and returns `fill` as the HUD progress value
  - ESC in Analysed mode calls `audio.set_live_paused(true)`; on resume, a skip fast-forwards the cursor and applies the last `Section`.
  - LiveSetup: replace the delay clamp at main.rs ~1124-1128 with presets `[0 (reactive), 12, 20, 30, 45, 60, 90, 120, 180]`.

**6. Per-app capture** (audio-dsp-engineer, can start after step 4)
- `Cargo.toml`: add `[target.'cfg(windows)'.dependencies] wasapi = "=0.27.0"` plus `windows` with features `Win32_System_Threading` and `Win32_System_Diagnostics_ToolHelp`, at the same version wasapi uses.
- New `src/capture.rs`:
  - `pub struct AudioApp { pid, root_pid, name, peak: f32, active: bool }`.
  - `pub fn list_audio_apps() -> Vec<AudioApp>`: enumerate sessions on all render devices, skip pid 0 and our own pid, map each pid to its top-most ancestor with the same exe (browser audio runs in child processes), dedupe, sort active-and-loud first.
  - `pub fn start_app_capture(pid, tree, sh: Arc<Shared>) -> Result<AppCapture, String>`: runs on its own thread (`wasapi::initialize_mta()`), separate from the cpal thread to avoid COM apartment clashes. It requests 48 kHz f32 stereo with autoconvert (process-loopback clients don't report a mix format), uses an event-driven read, calls `engine::push_frames`, and treats silent-flag packets as zeros.
- In `engine.rs`, `Source::App` uses it. If activation fails (older Windows, or the process has exited), fall back to `Device(None)` loopback and set status "per-app capture unavailable - capturing the whole device".
- Add `Cmd::ListApps`, which refreshes `Shared.apps`, and `Audio::apps()`.
- **Spike first (about 1 h, results go in PROJECT_STATE):**
  - (a) Does process loopback still capture when the app is muted with `ISimpleAudioVolume::SetMute` or the mixer?
  - (b) Does it capture when the app is routed to a different endpoint ("App volume and device preferences")?
  - (c) Minimum Windows build that works (MS documents 20348; reports say 19041 works).
- In `src/tools.rs`, `--live-probe` lists apps with peaks; add `--live-app <name-substring>`.

**7. LiveSetup screen rework** (gameplay-programmer)
- In `src/main.rs`, rewrite the `Screen::LiveSetup` block into three columns:
  - SOURCE: audio apps with live peak bars (refreshed every 1 s), then devices
  - MODE: Reactive / Look-ahead N s / Record & Play
  - PLAY ON: the playback device
- With app capture, playback may be the **same** device. There's no feedback loop, because only the target process is captured.
- If spike (a) works, add a toggle "Mute the app while playing" that mutes the session. It must be restored in `to_menu`, `stop_live` and the panic hook (store the pid in a global `AtomicU32`). Otherwise show routing instructions.
- `App` gains `src_sel` and `mode_sel` (persisted later).

**8. Record & Play mode** (gameplay-programmer)
- In `src/main.rs`, capture through the tap into a stereo buffer (needs `LiveTap::take_stereo`) until a silence ≥ `BOUNDARY_SILENCE` after at least 20 s of audio, or Enter, with a 10 min cap (about 230 MB as f32 stereo).
- Then build a `Track`, run `spawn_loader`-style background `analyze` + `generate` (reuse `load_track`'s back half as `load_samples(track, seed, diff, …)`), and continue through `begin_song`.
- Drift mode shows "RECORDING 2:31 - press ENTER when the song ends". Recordings stay in memory only.

**9. (Optional) Predictive reactive mode** (audio-dsp-engineer + gameplay-programmer)
- Run `RollingAnalyzer` with `GUARD=0` committing into the past, and extrapolate beats and bars forward from tempo and phase.
- `LiveGen` then uses predicted bars for tile strikes (with real telegraphs) and `beat_pos` for the gun, instead of the fixed 120 BPM grid.

**10. Docs** (either agent): update PROJECT_STATE.md (decisions, spike results, new CLI flags), DESIGN.md §2/§3 (pipeline diagram, modes) and README.

## 4. Tuning knobs
All live in `RollCfg` (analysis.rs) unless noted.
- `WINDOW_SECS` = 45: analysis context.
- `STRIDE` = 2 s: commit cadence.
- `GUARD` = clamp(0.25·delay, 6, 16) s, and never less than 4 bars.
- `MIN_ANALYSED_DELAY` = 12 s.
- `TEMPO_PRIOR_SIGMA` = 0.08 (log2 units).
- `OCTAVE_LOCK` = true.
- `DOWNBEAT_HYST` = 2 windows; phase-score decay 0.7 per window.
- `REF_HISTORY` = 180 s; `REF_EMA` = 0.3.
- `SECTION_SPLIT` = 26 s / 32 s hard.
- `BOUNDARY_SILENCE` = 1.5 s; `RUN_END_SILENCE` = 20 s (shows the boundary banner and orbs).
- `TEMPO_JUMP` = 8%.
- `PAUSE_MARGIN` = 30 s (engine.rs).
- Gap-fill threshold = 50 ms (engine.rs).
- Delay presets (main.rs).
- Record cap = 10 min.

## 5. Verification (for playtester)
Cargo: `%USERPROFILE%\.cargo\bin\cargo.exe`. Exe: `target\release\heartline.exe`. Make the test track first with `--gen-test tmp\test.wav` and convert it to mp3 with ffmpeg.

- **Every step:** `cargo test --release`. All old tests pass, plus the new ones: `frontend_chunked_matches_offline`, `analyze_golden` (step 1); `chunked_equals_whole` (step 2); `rolling_*` (step 3).
- **Steps 1-2:** `heartline.exe --analyze tmp\test.mp3`.
  - After step 1, the tempo, beats, onsets and sections lines must be **identical** to before.
  - After step 2, event counts should be within about 10% of before (seed and layout changed) and the level hash stable across two runs.
- **Step 3:**
  - `heartline.exe --live-sim tmp\test.mp3 --delay 30`: beat agreement ≥ 90%, BPM within 1.5 of offline, every offline section boundary matched within ±1 bar, a Drop present, window compute p50 < 15 ms, and the same hash on a second run.
  - Add `--live-sim-gap 20,26`: one boundary near 20 s, then beats resume.
  - Run on 2-3 real mp3s and record the numbers.
- **Step 4:**
  - `heartline.exe --live-probe 15` while pausing the music for 5 s: `captured` keeps growing at about sr/s through the pause.
  - Old flows still work: `--live --selftest tmp\live --wall --shots 5,9 --secs 10` and `--live-out Razer --mute --selftest tmp\dl --wall --shots 1.5,8 --secs 10`.
- **Step 5:** `heartline.exe --live-file tmp\test.mp3 --delay 20 --mute --selftest tmp\lf --wall --shots 5,25,40,52 --secs 60`.
  - 5 s: drift screen with a LISTENING progress bar and a BPM shown.
  - 25 s and later: tile strikes on bars, lasers, section palette changes; the drop shockwave around test-track drop + 20 s.
  - The log shows no "stale event" drops.
  - Add `--live-gap 30,36`: drift during the gap, then the level resumes.
- **Step 6:**
  - `heartline.exe --live-probe 5` lists audio apps (e.g. a browser or Spotify) with non-zero peak.
  - `--live-app spotify --delay 20 --selftest tmp\app --wall --shots 5,28 --secs 30` plays the level from that app only (play a system sound during the run; it must not register).
  - Report the spike results (a)/(b)/(c).
- **Step 7:** `heartline.exe --screen live --selftest tmp\setup --shots 1 --secs 2`: screenshot shows the app list with meters, mode presets and the playback column.
- **Step 8:** manual. Record a 1-minute song, press Enter; a "READING THE MUSIC" progress bar appears, then the level matches what `--analyze` gives on an mp3 of the same song.

## 6. Risks / open questions
- **Hearing the song twice (needs the user's input).** In delayed mode on a single output device, you hear the source app live and then Heartline's delayed copy. The fixes are: auto-mute the app (only if spike (a) shows capture still works when muted), route the app to an unused output (spike (b)), or the current two-device setup. Is auto-muting the app acceptable, given it must reliably restore after a crash?
- **Game pause in delayed mode (user's call).** The plan freezes the delayed playback while the source keeps playing, allows up to 30 s, then skips ahead. The alternative is no pause in live mode.
- **Delay range and default (user's call).** Proposed: reactive or 12-180 s, default 30 s. The old 1.5-6 s short delay is dropped, because sections need about 6 s of guard. Keep a short look-ahead (it would use the old `LiveGen`)?
- **Record & Play.** Keep recordings in memory only, or allow saving to disk? Plan says memory only.
- **Live levels won't match file levels exactly.** Windowed refs and section decisions with limited future differ from the offline result. Determinism promise: the same captured sample stream + seed + config gives the same events. `--live-sim` on a WAV checks this, because real capture timing (gap fill) isn't reproducible. Suggest a later `--live-record out.wav` to dump exactly what the analyser saw, for bug reports.
- **Skips without silence** (DJ crossfades) may not trigger a boundary until the tempo-jump rule fires 2 windows later (about 4 s).
- **`wasapi` churn.** It has had 12+ breaking releases, hence the exact pin. cpal and wasapi may pull different `windows` versions, which costs compile time only.
- **Precision.** f32 capture-timeline seconds are fine for sessions of a few hours. Rebase on boundaries if very long sessions matter.
- **Future LAN multiplayer (design for it now, don't build it).**
  - Keep `EventBatch` append-only with sequence numbers and times on one shared level timeline. Generation runs only on the host; clients never regenerate.
  - Keep `LevelEvent`/`Ev` plain `Copy` data, so a compact encoder is trivial later.
  - Keep `LevelGen` and `RollingAnalyzer` free of wall-clock or global state.
  - The worker → game boundary in step 5 is exactly the broadcast point.

New files: `src/live.rs`, `src/capture.rs`. Changed: `src/analysis.rs`, `src/level.rs`, `src/engine.rs`, `src/main.rs`, `src/tools.rs`, `Cargo.toml`.

## Sources
- [wasapi crate](https://docs.rs/crate/wasapi/latest) · [AudioClient docs](https://docs.rs/wasapi/latest/wasapi/struct.AudioClient.html) · [record_application example](https://docs.rs/crate/wasapi/latest/source/examples/record_application.rs) · [processes example](https://docs.rs/crate/wasapi/latest/source/examples/processes.rs) · [wasapi-rs GitHub](https://github.com/HEnquist/wasapi-rs)
- [Microsoft application loopback sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/) · [AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params) · [ActivateAudioInterfaceAsync](https://learn.microsoft.com/en-us/windows/desktop/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync) · [Audio Sessions](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audio-sessions)

---

# Review (Fable 5.2, 2026-11-13)

## Part 1: Approach and steps 1-3 (analysis + generator)
Line refs: `analysis.rs` = A, `level.rs` = L. `pick_onsets` (A:287-337) and `StreamAnalyzer` were not checked.

**Step 1 (front end)**
- Bit-identical per frame is achievable: `Spectral::process` only carries `prev`/`has_prev` (A:191, 206, 221), and the n/2 pre-pad matches A:493-497.
- **Missing `FrontEnd::finish()`.** `analyze` emits `len/hop + 1` frames, zero-padded at the end (A:485, 497). Without `finish()` the streaming version is short by about n/(2·hop) frames, and `frontend_chunked_matches_offline` fails on length. `front_end_all` must call `finish()`.
- The content seed is computed from `mono` (A:810-814); `front_end_all` or the `analyze` wrapper must compute it.

**Step 2 (chunked generator): chunked == whole will fail as written**
- **Shared state across passes.** `sec_rot`/`pulse_n` are advanced by both the onset pass (L:405-411) and the grid pass (L:463-466), so update order depends on where chunks split. Use one time-ordered merge loop over onsets, beats and bars inside `extend`, or give grid pulses their own counters. `last_spawn` is shared by Mid and High (L:421, 437), which is fine only if onsets stay in one ordered loop.
- **`near_bar` looks ahead** (L:381). `Chunk.beats` needs about 0.2 s of overlap past `t1`, or onsets near `t1` must be deferred.
- **Silences and phrases must arrive complete.** They need their full extent when they start: `in_silence` gating (L:460, 499) and the laser duration (L:492). State in the `Chunk` contract that silences and phrases starting in `[t0,t1)` arrive complete.
- **Sections can't change once they fire.** `Ev::Section` and `make_deck(kind)` fire at `s.t0` (L:333-337). The carried `sum/n` exists only for the next section's Drop comparison (A:772).
- **Smaller points:**
  - Tiles need their own bar counter (L:522) that survives a downbeat phase flip.
  - `grace`/`t_start` are first-chunk only (L:312-318); define what `boundary` does to them.
  - `beat_len` becomes per-chunk (L:310).
  - Use one persistent grid `Budget`, not one per section (L:458).
  - Put `Chunk` in level.rs so steps 1 and 2 don't both edit analysis.rs.

**Step 3 (rolling analyser)**
- **The tempo switch can't happen as designed.** `estimate_tempo` autocorrelates the whole window (A:347-354), so the old tempo dominates a 45 s window for about 20 s. Worse, a 0.08-log2 prior effectively forbids 96→140, so the tempo-jump boundary rule can never fire. Fix: estimate on a trailing 12-16 s slice, compute primed and unprimed estimates, and switch when they disagree for 2 windows.
- **The DP anchor is sound but needs real work.** Today any frame can start a chain (A:436-446). Frames in `(anchor, anchor+w_hi]` not reached from the anchor must be forced very negative. A wrong committed beat self-corrects after one odd interval.
- **Missing beat fallbacks.** Offline extrapolates a grid when beats are missing (A:553-578). The rolling path must continue the grid from the carried period and phase during beatless or ambient windows, or the gun and tiles stop.
- **GUARD is too small for the section rule.** The novelty test needs about 6 bars of future (A:706, 719): 12 s at 120 BPM, 16 s at 90 BPM. Either relax the live peak test (one-sided, b+1) or raise the minimum delay to about 20 s.
- **Cold start.** Bar z-scores and the novelty threshold (A:695-720) and intensity p10/p95 (A:648-653) are whole-song statistics. Early on, and after each boundary, they are noise and give spurious Intense/Drop sections. Fix: no novelty boundaries until about 12 bars of history, and blend in absolute-prior refs by history length.
- Make commit boundaries frame indices (u64), not `f32` seconds.

**Simpler alternative (recommended).** Drop the 45 s window. Beats before the anchor are frozen and refs/bar history live in Carry, so each stride only needs to process `[anchor - ~3 s, head)`, with tempo taken from a trailing 16 s slice. This removes the slow tempo switch and most seam logic, and cuts compute.

**Missing tests**
- `chunked_equals_whole`: add splits near a bar, inside a silence, inside a phrase and at a section start, plus a fixture with a kickless section.
- `analyze_golden`: also hash `feats`, `intensity` and the `generate` output.
- New `rolling_*` tests: beatless input, quiet-intro cold start, downbeat phase stability.
- `--live-sim`: report the tempo octave ratio, and section-kind agreement for the first 45 s separately.

**Can cut or defer**
- `trim_before`/`base_frame` (move to step 3).
- The tempo-jump song boundary (ship silence-only boundaries).
- `K_GRID` (if grid pulses get their own counters).
- The duplicate `--analyze` check after step 1.
- Octave lock (add it only if `--live-sim` shows octave flips).

## Part 2: Steps 4 and 6 (engine + per-app capture)
Line refs are `engine.rs`. Claims about wasapi/cpal internals marked (unverified) come from the reviewer's memory, not research.

**Step 4: silence insertion**
- **Late bursts double-count time.** If the capture callback stalls for more than 50 ms and then delivers its backlog, the tick has already written zeros, so the timeline gets zeros plus the data: a dropout in the music and a shifted beat grid.
  - Fix: a threshold of about 200 ms, and fill only up to `now - 100 ms`.
  - Better: decide padding at packet arrival from the packet timestamp or discontinuity flag (cpal `InputCallbackInfo` / wasapi buffer flags, unverified).
- **Don't use cumulative accounting** (`elapsed*sr - write_abs`). Device drift would insert false zeros after about 17 minutes.
- **The tick stalls with the audio thread** (:126: `refresh_devices`, stream builds, `ListApps`).
- **The tick must not touch `last_write`.** `since_packet` (:267) and the reactive `live_time` (:282) use it. Add `last_fill`.
- **The File source fights the tick.** Disable the tick for File, pace the feeder by a cumulative schedule, and push explicit zeros for gaps.

**Step 4: pause and skip (not sound as written)**
- **Drift correction turns a pause into a long pitch shift.** It's clamped at +0.4% (:563), so after a 20 s pause, playback runs fast for about 80 minutes to catch up.
- **A second pause skips**, because the margin is already spent (:542).
- Fix, pick one:
  - (a) On resume, set `delay_frames = avail`, so the delay grows. Skip only when the margin runs out. Best feel.
  - (b) Always jump back to the target delay on resume. Least code; no `PAUSE_MARGIN` needed.
- **Gate `live_time` extrapolation (:278) on `paused`.**
- **Replace `LiveInfo.skipped`** with a monotonic counter, or detect the jump in `live_time()`.
- **Leave `rebuffering` semantics alone.** It's the safety net for a dead capture stream.

**Step 4: ring and locking**
- **Ring size is fine.** 210 s at 48 kHz is about 81 MB, but allocate it outside the `live` mutex and swap it in (:446-450).
- **Watch the shared mutex.** The tick, the tap and the wasapi thread all join both callbacks and the main thread on one `Mutex<LiveState>`.
  - Reserve before locking and cap frames per hold in `live_take` (:296, :301).
  - In the callbacks (:481, :526), use `lock().unwrap_or_else(|e| e.into_inner())` so a worker panic doesn't kill audio.
  - A lock-free ring isn't needed yet.
- **Extract `init_live(sr, delay, delayed)`** from `build_capture_stream` (:441-461) so the File and App sources can use it.
- **The `delayed` rule at :159 needs an explicit version for the File and App sources.**

**Step 6: per-app capture**
- **Don't fall back to whole-device capture.** With same-device playback that's a feedback loop, and the :159 guard silently drops to reactive mode. Return to LiveSetup with a message instead.
- **COM.**
  - A separate MTA thread is fine if the `AudioClient` lives and dies on it.
  - Don't run `ListApps` on the cpal thread: cpal likely uses STA there, so `initialize_mta` fails (unverified).
  - Put session enumeration on one long-lived wasapi thread in `capture.rs`.
- **Lifecycle.**
  - Give `AppCapture` a stop flag and join it before the next `StartLive` resets the ring (:144-146).
  - A generation counter in `push_frames` stops stale writes.
- **Wait on capture events with a ~100 ms timeout.** Process-loopback events may not fire during silence (unverified).
- **Detect when the target app exits.** Poll its process handle and show a status.
- **Exclude Heartline's ancestors** (launcher, terminal) from the app list, since `tree=true` would capture Heartline itself.
- **Per-app capture alone does not fix hearing the song twice.** That depends on spikes (a) and (b).
- **Auto-mute survives crashes.** Windows persists per-app mute. Write a marker file on mute and restore it at the next launch.

**Better alternatives**
- **Run the spike before anything else.** If (a) and (b) both fail, single-device delayed mode isn't viable and step 7's layout changes.
- **Pause:** choose (a) or (b). Don't ship the current hybrid.
- **Extract pure, testable functions:** `fill_due(now, last_write, last_fill, sr)` and `render_live(&mut LiveState, out, out_sr)`.

**Missing tests**
- `fill_due`: a stall followed by a burst adds no zeros; a 5 s gap adds about 5 s; ±200 ppm drift over an hour adds none.
- `render_live`: pause hold, resume with no pitch chase, overflow skip, `live_time` frozen while paused.
- `--live-probe`: print the fill count against wall time (10 minutes of music must show 0 fills).
- App capture: target exits mid-run, 20 start/stop cycles, same-device playback with no echo, activation failure returns to setup.

**Can cut**
- `LiveInfo.skipped`.
- The `rebuffering` change.
- Spike (c): the user is on Windows build 26200.
- The `tree` parameter (always true).
- `ListApps` on the audio thread.
- `--live-gap` and the gaps list in `Source::File` (if `fill_due` is unit-tested).
- `PAUSE_MARGIN` (if pause option (b) is chosen).

## Part 3: Steps 5, 7, 8 and multiplayer notes (game integration)
Line refs: `main.rs` = M, `game.rs` = G. `engine.rs`, `feat_at`, `beat_pos` and `World::apply` (beyond the portal arm) were not checked.

**Step 5: scheduling**
- **Define a late-event policy.** Reactive drops events more than 1 s stale (M:724), but the cursor loop copied from `step_files` (M:544-548) fires everything due, with no hitch clamp (M:541 vs M:672). After a stall, seconds of spawns would land in one frame.
  - `e.t < clock`: drop, but still apply the latest `Section`.
  - `at() < clock <= e.t`: apply, with a shortened telegraph. Portals use absolute times (G:506); check the other arms do too.
- **"Merge-insert only the tail" is wrong.** A new batch can hold events whose `at()` falls up to 1.3 s before the previous batch's last events, or before the cursor, so they'd be skipped silently. Fix: append, then `events[cursor..].sort_by(at)` once per batch.
- **Fired events pile up.** `events` and `beats` grow for hours. Use a `VecDeque` and pop the fired prefix.
- **Define what happens when the worker falls behind.** If `analysed_until < clock + 1.5`: drift mode with status "CATCHING UP".
  - Handle `Disconnected` (worker panic) with a status.
  - Wrap the worker in `catch_unwind`, as `spawn_loader` does (M:209).
- **GUARD exceeds the look-ahead budget.** At a 12 s delay, `delay - GUARD - STRIDE` goes negative. Cap GUARD at `delay - STRIDE - 2.5`, or make the minimum preset 20 s.
- **`EventBatch` needs silences**, or a per-feat active flag, for the idle check (file mode uses M:549).
- **`feat_at` can't be reused.** It takes `&Analysis` (M:550), and a linear scan over the feat deque is too slow. Index by `(clock - front_t) / hop_dt`, and check `beat_pos` isn't a linear scan.

**Step 5: next-track boundary**
- **Make the boundary a timed `Ev`.** A `boundary: bool` on the batch arrives `delay - GUARD` seconds before it's audible.
- **"begin_song" means `World::begin_song` (G:420), not `App::begin_song` (M:448).**
- **A 1.5 s silence is too short for a full reset.** Use 1.5 s for calm only. A 3-4 s silence gives `Ev::End` (bonus, `stats.songs`, G:555-560), orbs and a new heart.
- **Live `active_secs` is hardcoded to 210** (M:385, 711). Needs a decision.

**Step 5: split, threading, pause**
- **Keep a shared prelude in `step_live`:** the Dying block (M:627-639), `!info.running` (M:643-648) and the dead check (M:747) apply to both modes. Dispatch after them.
- **Spawn the worker lazily when `info.running` arrives** (M:649); `sr` isn't known in `start_live`.
- **One capture consumer per run.** Reactive drains with `live_take` (M:653); the worker uses the tap.
- **Implement `Drop` for `LiveWorker`** (stop + join) rather than patching `self.live = None` sites (M:342, 357, 635). The worker loop needs a sleep or condvar, not a spin.
- **ESC.**
  - Live ESC currently calls `to_menu()` (M:943-944).
  - The Paused screen is file-only: Resume calls `audio.set_paused(false)` (M:1153), and RESTART calls `start_run(queue)` (M:1156-1157).
  - Both need live handling.
- **Dropping the 1.5-6 s presets leaves dead code** (M:386, 695-696, 735-741). Keep one short preset or delete that code.

**Step 7**
- **Select apps by `root_pid`, not by index** (M:376-378), and sort stably by name. Re-sorting by loudness every second moves rows under the cursor.
- **Enumerate apps on the `capture.rs` thread**, not with `Cmd::ListApps` on the audio thread.
- **The lists need a new layout.** They're `take(9/10)` with no scroll (M:1069, 1085) and `col_w` is 800u.
- **Keep the same-device rule for Device sources only** (M:379, 1086, 1097-1099); relax it for App sources. Specify the valid combinations.
- **Use one RAII mute guard owned by the engine's live state, not the panic hook (M:145).** The hook misses kills, Dying to Results (M:633), window close and selftest exit (M:1243).

**Step 8 (Record & Play)**
- **Memory.** The 230 MB buffer, plus `Vec` doubling, plus `track.mono()` (M:197) adds up. Preallocate or use i16, and cap at about 6 min.
- **Record engine-side** in `push_frames` and expose `Audio::take_recording()`, not on the main thread.
- **Flow.** The `load_track` reuse is three lines (M:197-200), but loading is queue-indexed (M:423, 439-444, 509-516, 938).
  - After recording: set `live = None`, push a pseudo queue entry, attach a samples loader and set `phase = Loading`.
  - Keep the `Arc<Track>` for Restart (M:1156).
- **Auto-stop at about 3 s of silence**, not 1.5 s, and trim leading and trailing silence.
- **Define what happens after the song:** Results, or record again.

**Better alternatives**
- **Pause (user's call).** Freeze only the simulation and let the delayed audio keep playing. On resume, `calm_down` and fast-forward the cursor (factor out M:453-464). This removes engine pause and skip, and `PAUSE_MARGIN`.
- **Ship step 8 before step 5.** It only needs an engine recording buffer and gives file-quality levels right away.
- **Presets.** Step 5 only adds `--delay`; the preset UI goes in step 7.

**Missing tests**
- `live.rs` unit tests: out-of-order batch merge, the late policy, deque trimming.
- Selftests:
  - `--live-file` with an injected stall (`--live-stall 30,4`).
  - `--live-file --delay 0` (reactive unchanged).
  - ESC/pause.
- Step 7: add `--fake-apps` and `--live-mode N` so screenshots don't depend on the machine's audio.
- Step 8: `--live-file X --record --selftest`, asserting the level hash equals `--analyze X`.

**Cut or defer**
- Head-of-capture feats during the fill (use `Feat::default()` as Loading does, M:528).
- The mute toggle (a separate step after the spike).
- Engine pause and skip (if the pause alternative is chosen).
- Live peak meters (a static active flag in v1).
- Multi-song Record & Play.

**Multiplayer notes:** fine in direction. Additions:
- Use u64 frame indices for batch `t0`/`t1`.
- Carry the boundary as a timed event.
- Give each batch a small state header (section, bpm, absolute beat index) for late joiners and pause-resume.
- Not yet addressed: clients need the delayed audio stream.
- Not yet addressed: `World` has its own RNG (M:365), so the simulation isn't lockstep-ready. Note it, don't solve it now.

## Review summary: decisions before implementation
1. **Run the per-app capture spike first** (mute + rerouting). It decides whether single-device delayed mode is possible and shapes step 7.
2. **Analysis design:** adopt the reviewer's simpler stride design (no 45 s window), and fix tempo switching, the generator merge loop, cold start and GUARD.
3. **Minimum delay:** about 20 s (or a relaxed section test that allows 12 s).
4. **Pause:** sim-only pause (simplest), or option (a) or (b) from Part 2. Not the current hybrid.
5. **Order:** consider shipping Record & Play (step 8) right after step 4.

---

# Spike results: per-app capture (2026-11-13)
Built as a C#/.NET 10 test (Rust isn't installed on this machine), making the same Win32 calls the `wasapi` crate wraps. Project: the session scratchpad `app-capture-spike` (throwaway). **Not yet checked:** which `wasapi` crate version and API names work. Verify on a machine with cargo.

| Case | Result |
|---|---|
| Capture the player's PID (include tree) | Works: -27.4 dBFS RMS, -13.2 dBFS peak |
| Another process while the player plays | Not captured (exact zeros) |
| (a) Session `SetMute(true)` | **Capture goes silent** (exact zeros after a short fade) |
| (a) Session volume 0 | **Capture goes silent** |
| Restore | Back to normal; mute and volume restored |
| (b) Route the app to another output | Not tested; there's no documented API (only the undocumented `IAudioPolicyConfigFactory`) |
| (c) Windows build 26200 | Activation works |

**Conclusions**
- **Auto-mute is not viable.** Process loopback captures after session mute and volume are applied. Drop the mute toggle, its marker file and the panic-hook restore from step 7.
- **Single-device delayed mode depends on (b).** The user routes the music app to an unused output in Windows "App volume and device preferences". Otherwise it's two output devices, reactive mode, or Record & Play.
- **Step 7 shows routing instructions** instead of a mute toggle.

**Facts for implementation**
- `GetMixFormat` returns E_NOTIMPL. Request 48 kHz, 2 ch, f32 with `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`; it works first try. Packets are 10 ms (buffer of 480 frames).
- Activation takes 2.5-6.9 ms the first time and about 0.1 ms after that; Initialize + Start takes 0.2-2.8 ms.
- **Capture events keep firing every 10 ms during silence**, including when the target is suspended or killed. Silence arrives as real zero samples, and the SILENT/DISCONTINUITY flags never appear. Keep a 100 ms wait timeout anyway.
- **Bad or exited PIDs don't fail.** Activation succeeds and streams zeros, so "activation fails → message" won't catch them. Poll the process handle.
- **The capture client appears as its own session.** Exclude Heartline's PID and its ancestors from the app list.
- **Mute ramps over tens of milliseconds**, and the session peak meter can read non-zero while muted (it isn't a mute indicator).
- Session enumeration takes 3-16 ms. Example row: `pid=18272 ffplay.exe Active peak=0.072`.
