//! Clip decoding via symphonia: wav / ogg-vorbis / mp3 / flac → f32 PCM.
//! Decode happens once on the control side (load or first play); the audio
//! thread only ever sees ready `Clip`s.

use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use crate::clip::Clip;

/// File extensions the decoder accepts (lowercase, no dot).
pub const AUDIO_EXTENSIONS: &[&str] = &["wav", "ogg", "mp3", "flac"];

/// True if the path looks like a loadable audio file.
pub fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Decode an entire audio file into a clip. Sources with more than two
/// channels keep their front left/right pair.
pub fn load_clip(path: &Path) -> Result<Clip, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("unrecognized audio format {}: {e}", path.display()))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| format!("{}: no audio track", path.display()))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| format!("{}: no audio codec parameters", path.display()))?
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| format!("{}: unsupported codec: {e}", path.display()))?;

    let mut sample_rate = 0u32;
    let mut src_channels = 0usize;
    let mut samples: Vec<f32> = Vec::new();
    let mut packet_pcm: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => return Err(format!("{}: read error: {e}", path.display())),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            // A corrupt packet shouldn't kill the whole clip.
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("{}: decode error: {e}", path.display())),
        };
        if buf.is_empty() {
            continue;
        }
        let spec = buf.spec();
        sample_rate = spec.rate();
        src_channels = spec.channels().count();
        buf.copy_to_vec_interleaved(&mut packet_pcm);
        if src_channels <= 2 {
            samples.extend_from_slice(&packet_pcm);
        } else {
            // Multichannel: keep the front pair.
            samples.extend(
                packet_pcm.chunks_exact(src_channels).flat_map(|frame| [frame[0], frame[1]]),
            );
        }
    }

    if samples.is_empty() || sample_rate == 0 {
        return Err(format!("{}: decoded no audio", path.display()));
    }
    Ok(Clip { sample_rate, channels: src_channels.min(2) as u16, samples })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Minimal 16-bit PCM WAV writer for the test fixture.
    fn write_wav(path: &Path, sample_rate: u32, samples: &[i16]) {
        let mut f = File::create(path).unwrap();
        let data_len = (samples.len() * 2) as u32;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        for s in samples {
            f.write_all(&s.to_le_bytes()).unwrap();
        }
    }

    #[test]
    fn decodes_wav() {
        let dir = std::env::temp_dir().join("floptle-audio-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        let sr = 22_050u32;
        let src: Vec<i16> = (0..sr / 10)
            .map(|i| ((std::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin() * 12_000.0) as i16)
            .collect();
        write_wav(&path, sr, &src);

        let clip = load_clip(&path).expect("wav should decode");
        assert_eq!(clip.sample_rate, sr);
        assert_eq!(clip.channels, 1);
        assert_eq!(clip.frames(), src.len());
        assert!(clip.samples.iter().any(|s| s.abs() > 0.3), "signal lost in decode");
        assert!(is_audio_path(&path));
        assert!(!is_audio_path(Path::new("foo.png")));
    }
}
