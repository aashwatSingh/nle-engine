//! Runs the prebuilt `whisper-cli.exe` as a subprocess against a WAV file and
//! parses its JSON output. See `lib.rs`'s module doc for why this is a
//! subprocess rather than an in-process FFI link.

use crate::transcript::{parse_whisper_json, ParseError, Transcript};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the prebuilt binary and model live on this machine. Hardcoded, not
/// discovered — the same trade this project already made for FFmpeg
/// (`.cargo/config.toml`'s `FFMPEG_DIR`, noted there as "machine-specific,
/// not portable"): a portable install story is real follow-up work, not
/// something to half-solve with a search path that would fail silently on a
/// different machine anyway.
pub struct WhisperConfig {
    pub cli_path: PathBuf,
    pub model_path: PathBuf,
}

impl Default for WhisperConfig {
    fn default() -> Self {
        WhisperConfig {
            cli_path: PathBuf::from(
                r"C:\Users\aashw\tools\whisper-bin-x64\extracted\Release\whisper-cli.exe",
            ),
            model_path: PathBuf::from(r"C:\Users\aashw\tools\whisper-models\ggml-base.en.bin"),
        }
    }
}

#[derive(Debug)]
pub enum TranscribeError {
    /// The CLI binary or model file isn't where `WhisperConfig` says.
    NotInstalled(PathBuf),
    Io(std::io::Error),
    /// The CLI ran but exited non-zero — stderr is included since whisper-cli
    /// prints its actual failure reason there, not in the exit code.
    CliFailed { status: Option<i32>, stderr: String },
    Parse(ParseError),
}

impl std::fmt::Display for TranscribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranscribeError::NotInstalled(p) => write!(f, "whisper not installed: {} not found", p.display()),
            TranscribeError::Io(e) => write!(f, "io error running whisper: {e}"),
            TranscribeError::CliFailed { status, stderr } => {
                write!(f, "whisper-cli exited with {status:?}: {stderr}")
            }
            TranscribeError::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TranscribeError {}

/// Writes 16-bit PCM mono/stereo `samples` (interleaved if `channels == 2`)
/// as a standard WAV file. whisper.cpp's reader (miniaudio) resamples
/// internally, so this doesn't need to match whisper's native 16kHz mono —
/// verified against a real 48kHz stereo file in
/// `tests/whisper_real_footage.rs`'s smoke test.
fn write_wav_i16(path: &Path, samples: &[f32], channels: u16, sample_rate: u32) -> std::io::Result<()> {
    let bits_per_sample: u16 = 16;
    let block_align = channels * (bits_per_sample / 8);
    let byte_rate = sample_rate * block_align as u32;
    let data_bytes = samples.len() as u32 * 2;

    let mut buf = Vec::with_capacity(44 + data_bytes as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_bytes.to_le_bytes());
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let i16_sample = (clamped * i16::MAX as f32).round() as i16;
        buf.extend_from_slice(&i16_sample.to_le_bytes());
    }
    std::fs::write(path, buf)
}

/// Transcribes `samples` (interleaved PCM at `sample_rate`, `channels`
/// channels) via the prebuilt whisper.cpp CLI, returning the parsed
/// `Transcript`. Writes a temp WAV file, invokes the CLI synchronously
/// (transcription is a one-shot "analyse this clip" action, not a
/// low-latency real-time path — a blocking subprocess call is the right
/// shape here, the same judgement call every other analysis action in this
/// project made for its own decode-and-crunch step), reads back its JSON
/// output, and cleans up the temp files regardless of outcome.
pub fn transcribe(
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
    config: &WhisperConfig,
) -> Result<Transcript, TranscribeError> {
    if !config.cli_path.is_file() {
        return Err(TranscribeError::NotInstalled(config.cli_path.clone()));
    }
    if !config.model_path.is_file() {
        return Err(TranscribeError::NotInstalled(config.model_path.clone()));
    }

    let temp_dir = std::env::temp_dir();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let wav_path = temp_dir.join(format!("nle_transcribe_{unique}.wav"));
    let output_stem = temp_dir.join(format!("nle_transcribe_{unique}"));
    let json_path = temp_dir.join(format!("nle_transcribe_{unique}.json"));

    write_wav_i16(&wav_path, samples, channels, sample_rate).map_err(TranscribeError::Io)?;

    let run = (|| {
        let output = Command::new(&config.cli_path)
            .arg("-m")
            .arg(&config.model_path)
            .arg("-f")
            .arg(&wav_path)
            .arg("-oj")
            .arg("-ojf")
            .arg("-np") // no progress spam — only the JSON file matters
            .arg("-of")
            .arg(&output_stem)
            .output()
            .map_err(TranscribeError::Io)?;

        if !output.status.success() {
            return Err(TranscribeError::CliFailed {
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let json = std::fs::read_to_string(&json_path).map_err(TranscribeError::Io)?;
        parse_whisper_json(&json).map_err(TranscribeError::Parse)
    })();

    let _ = std::fs::remove_file(&wav_path);
    let _ = std::fs::remove_file(&json_path);
    run
}
