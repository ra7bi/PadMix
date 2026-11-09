//! Load a sound file into memory: decode (symphonia), make stereo, resample
//! to the engine rate (rubato). Done off the audio thread.

use anyhow::{anyhow, Context, Result};
use rubato::{FftFixedIn, Resampler};
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decoded audio at its original rate, stereo interleaved.
pub struct Decoded {
    pub rate: u32,
    pub stereo: Vec<f32>,
}

pub const SUPPORTED_EXT: &[&str] = &["mp3", "wav", "ogg", "flac", "m4a", "aac", "mp4"];

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED_EXT.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn decode(path: &Path) -> Result<Decoded> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .context("unsupported or corrupt audio file")?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("no audio track"))?
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("no decoder for this codec")?;

    let mut rate = track.codec_params.sample_rate.unwrap_or(0);
    let mut stereo: Vec<f32> = Vec::new();
    let mut buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(e).context("reading audio"),
        };
        if packet.track_id() != track.id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue, // skip a bad frame
            Err(e) => return Err(e).context("decoding audio"),
        };
        let spec = *decoded.spec();
        rate = spec.rate;
        let ch = spec.channels.count().max(1);
        let sb = buf.get_or_insert_with(|| SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        if sb.capacity() < decoded.capacity() * ch {
            *sb = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        }
        sb.copy_interleaved_ref(decoded);
        for frame in sb.samples().chunks_exact(ch) {
            let (l, r) = if ch == 1 { (frame[0], frame[0]) } else { (frame[0], frame[1]) };
            stereo.push(l);
            stereo.push(r);
        }
    }
    if rate == 0 || stereo.is_empty() {
        return Err(anyhow!("file contains no audio"));
    }
    Ok(Decoded { rate, stereo })
}

/// Convert stereo interleaved audio from `from` Hz to `to` Hz.
pub fn resample(stereo: &[f32], from: u32, to: u32) -> Result<Vec<f32>> {
    if from == to {
        return Ok(stereo.to_vec());
    }
    let frames = stereo.len() / 2;
    let mut chans = [Vec::with_capacity(frames), Vec::with_capacity(frames)];
    for f in stereo.chunks_exact(2) {
        chans[0].push(f[0]);
        chans[1].push(f[1]);
    }

    const CHUNK: usize = 1024;
    let mut rs = FftFixedIn::<f32>::new(from as usize, to as usize, CHUNK, 2, 2)
        .map_err(|e| anyhow!("resampler: {e}"))?;
    let delay = rs.output_delay();
    let expected = (frames as u64 * to as u64 / from as u64) as usize;
    let mut out = [Vec::with_capacity(expected + delay), Vec::with_capacity(expected + delay)];

    let mut pos = 0;
    while pos + CHUNK <= frames {
        let block = [&chans[0][pos..pos + CHUNK], &chans[1][pos..pos + CHUNK]];
        let res = rs.process(&block, None).map_err(|e| anyhow!("resample: {e}"))?;
        out[0].extend_from_slice(&res[0]);
        out[1].extend_from_slice(&res[1]);
        pos += CHUNK;
    }
    if pos < frames {
        let block = [&chans[0][pos..], &chans[1][pos..]];
        let res = rs.process_partial(Some(&block), None).map_err(|e| anyhow!("resample: {e}"))?;
        out[0].extend_from_slice(&res[0]);
        out[1].extend_from_slice(&res[1]);
    }
    // flush the filter tail until we have everything (plus the known delay)
    let mut guard = 0;
    while out[0].len() < expected + delay && guard < 16 {
        let res = rs
            .process_partial(None::<&[&[f32]]>, None)
            .map_err(|e| anyhow!("resample: {e}"))?;
        out[0].extend_from_slice(&res[0]);
        out[1].extend_from_slice(&res[1]);
        guard += 1;
    }

    let end = (delay + expected).min(out[0].len());
    let start = delay.min(end);
    let mut inter = Vec::with_capacity((end - start) * 2);
    for i in start..end {
        inter.push(out[0][i]);
        inter.push(out[1][i]);
    }
    Ok(inter)
}

/// Gain that brings a clip to a common loudness (about -20 dBFS RMS) without
/// clipping its peaks. Quiet clips are boosted at most 4x; silence is left alone.
pub fn loudness_gain(stereo: &[f32]) -> f32 {
    const TARGET_RMS: f32 = 0.1;
    const MAX_BOOST: f32 = 4.0;
    if stereo.is_empty() {
        return 1.0;
    }
    let (mut sum, mut peak) = (0.0f64, 0.0f32);
    for &s in stereo {
        sum += (s as f64) * (s as f64);
        peak = peak.max(s.abs());
    }
    let rms = (sum / stereo.len() as f64).sqrt() as f32;
    if rms < 1e-4 || peak < 1e-4 {
        return 1.0;
    }
    (TARGET_RMS / rms).min(0.98 / peak).min(MAX_BOOST)
}

/// Apply a gain in place.
pub fn apply_gain(data: &mut [f32], gain: f32) {
    if (gain - 1.0).abs() > 1e-4 {
        for s in data {
            *s *= gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loudness_boosts_quiet_and_tames_loud_without_clipping() {
        let quiet = vec![0.02f32; 2000];
        let g = loudness_gain(&quiet);
        assert!((g - 4.0).abs() < 1e-3, "quiet boost capped at 4x, got {g}");
        let loud: Vec<f32> = (0..2000).map(|i| if i % 2 == 0 { 0.9 } else { -0.9 }).collect();
        let g = loudness_gain(&loud);
        assert!(g < 0.2, "loud clip turned down, got {g}");
        let spiky: Vec<f32> = (0..2000).map(|i| if i == 10 { 1.0 } else { 0.01 }).collect();
        let g = loudness_gain(&spiky);
        assert!(g * 1.0 <= 0.98 + 1e-6, "peak must stay below 0 dBFS, got {g}");
        assert_eq!(loudness_gain(&[0.0; 100]), 1.0);
    }

    #[test]
    fn resample_length_is_proportional() {
        let src = vec![0.1f32; 44_100 * 2]; // 1 s stereo at 44.1k
        let out = resample(&src, 44_100, 48_000).unwrap();
        let frames = out.len() / 2;
        assert!((47_900..=48_000).contains(&frames), "{frames}");
    }

    #[test]
    fn resample_preserves_dc_level() {
        let src = vec![0.5f32; 22_050 * 2];
        let out = resample(&src, 22_050, 48_000).unwrap();
        let mid = &out[out.len() / 2 - 100..out.len() / 2 + 100];
        for s in mid {
            assert!((s - 0.5).abs() < 0.01, "{s}");
        }
    }
}
