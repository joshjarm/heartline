//! Audio file decoding (symphonia) into interleaved stereo f32.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

pub struct Track {
    pub name: String,
    pub path: PathBuf,
    pub sr: u32,
    /// Interleaved stereo.
    pub samples: Vec<f32>,
}

impl Track {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }
    pub fn duration(&self) -> f32 {
        self.frames() as f32 / self.sr.max(1) as f32
    }
    pub fn mono(&self) -> Vec<f32> {
        self.samples.chunks_exact(2).map(|c| 0.5 * (c[0] + c[1])).collect()
    }
}

pub const AUDIO_EXTS: &[&str] = &["mp3", "flac", "ogg", "oga", "wav", "m4a", "aac", "mp4", "aiff", "aif", "opus", "mka", "webm", "caf"];

pub fn is_audio_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Decode a whole file. `progress` receives 0..=1000 (per-mille) as a rough estimate.
pub fn decode_file(path: &Path, progress: &AtomicU32) -> Result<Track, String> {
    let file = File::open(path).map_err(|e| format!("open failed: {e}"))?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0).max(1);
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let fmt_opts: FormatOptions = Default::default();
    let meta_opts: MetadataOptions = Default::default();
    let dec_opts: AudioDecoderOptions = Default::default();

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, fmt_opts, meta_opts)
        .map_err(|e| format!("unrecognised audio format: {e}"))?;

    let track = format.default_track(TrackType::Audio).ok_or("no audio track")?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or("no audio codec parameters")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &dec_opts)
        .map_err(|e| format!("unsupported codec: {e}"))?;
    let track_id = track.id;

    let mut out: Vec<f32> = Vec::new();
    let mut tmp: Vec<f32> = Vec::new();
    let mut sr = 0u32;
    let mut bytes_seen: u64 = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(_) => break,
        };
        if packet.track_id != track_id {
            continue;
        }
        bytes_seen += packet.data.len() as u64;
        progress.store(((bytes_seen * 1000) / file_len).min(1000) as u32, Ordering::Relaxed);

        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = buf.spec();
                let ch = spec.channels().count().max(1);
                if sr == 0 {
                    sr = spec.rate();
                }
                tmp.resize(buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut tmp[..]);
                match ch {
                    1 => {
                        for &s in &tmp {
                            out.push(s);
                            out.push(s);
                        }
                    }
                    2 => out.extend_from_slice(&tmp),
                    _ => {
                        for fr in tmp.chunks_exact(ch) {
                            out.push(fr[0]);
                            out.push(fr[1]);
                        }
                    }
                }
            }
            Err(Error::DecodeError(_)) => (),
            Err(_) => break,
        }
    }

    if sr == 0 || out.len() < 2 * 1024 {
        return Err("no audio could be decoded".into());
    }
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled")
        .to_string();
    Ok(Track { name, path: path.to_path_buf(), sr, samples: out })
}
