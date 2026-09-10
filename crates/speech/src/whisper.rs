//! Runs the prebuilt `whisper-cli.exe` as a subprocess against a WAV file and
//! parses its JSON output. See `lib.rs`'s module doc for why this is a
//! subprocess rather than an in-process FFI link.

use crate::transcript::{parse_whisper_json, ParseError, Transcript};
use integrity::Pin;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the prebuilt binary and model live, relative to
/// `integrity::tools_dir`. Fixed, not discovered — the same trade this project
/// already made for FFmpeg (`.cargo/config.toml`'s `FFMPEG_DIR`, noted there as
/// "machine-specific, not portable"): a portable install story is real
/// follow-up work, not something to half-solve with a search path that would
/// fail silently on a different machine anyway.
const WHISPER_RELEASE_DIR: &str = r"whisper-bin-x64\extracted\Release";
const WHISPER_MODEL: &str = r"whisper-models\ggml-base.en.bin";

/// SHA-256 of every file in the whisper.cpp release that `whisper-cli.exe`
/// runs or loads, as vetted (from whisper-bin-x64.zip, itself SHA-256
/// 49dcc16de826f20b…, downloaded 2026-08-14). The CLI imports whisper.dll and
/// ggml.dll, which import ggml-base.dll; at startup it also loads whichever
/// ggml-cpu-*.dll matches this CPU, so every variant is pinned, not just the
/// one this machine happens to pick. See the `integrity` crate for why, and
/// docs/security.md for re-pinning after a deliberate upgrade.
const WHISPER_BINARIES: [(&str, &str); 13] = [
    ("whisper-cli.exe", "95e3c0b0e778ad9499eb0125f97c1dcf437dd9eb4ea77050b043574f93c2631d"),
    ("whisper.dll", "792fc523c7ad16e6b9c348e30ad5e5f591165cbcf6a80ca8d0db02a38ce3eea2"),
    ("ggml.dll", "894c6237ee7849843213906a2b6a0b371aaa6234048d465f206d910ae846fafb"),
    ("ggml-base.dll", "1482359d921b4c1b183d49db1d770f9b5e90d86a618b8b648d4845c2471ad6b0"),
    ("ggml-cpu-alderlake.dll", "d1c5411561361f7ce71ff8455ecf01f666f581b0608fa91a1dfe7d3fd6a25bd1"),
    ("ggml-cpu-cannonlake.dll", "2ef36f05fa252ff4fdcb8d42ebce1ceba4f3d3de12b93bed15bdee6237dccd63"),
    ("ggml-cpu-cascadelake.dll", "505899aaf3f99c5d714361640f561458ea97f8a09eb0614568a66bead2115cb0"),
    ("ggml-cpu-haswell.dll", "f8cf2f35a06498d783d77fde42004dd54d2f8236b0d42ac323b94bba65a603c4"),
    ("ggml-cpu-icelake.dll", "78ad143ee2e674d037b4840ef33b5748a0659762a26e0ae2b621c4f9451cbde8"),
    ("ggml-cpu-sandybridge.dll", "ee47db7dc40fb30eca73e62a05306059c2c3c42aecddf2e8d6ad7e530069b815"),
    ("ggml-cpu-skylakex.dll", "164e2793897944a43ee071ce6c0b09018088bdf4dd8b14ac0755c58849cf8c50"),
    ("ggml-cpu-sse42.dll", "7318a9a3b95a85b2453c437b274412bbbae89e5ecdf5babb19b99edc06ded063"),
    ("ggml-cpu-x64.dll", "af0f1c2f28ff9e3f472481dd969907bda85fa39d4fde17617d4bb0b389301b60"),
];
const WHISPER_MODEL_SHA256: &str = "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002";

pub struct WhisperConfig {
    pub cli_path: PathBuf,
    pub model_path: PathBuf,
    /// Files that must match their hashes before `cli_path` is run — the CLI,
    /// every DLL it can load, and the model it's handed.
    pub pins: Vec<Pin>,
}

impl Default for WhisperConfig {
    fn default() -> Self {
        let tools = integrity::tools_dir();
        let dir = tools.join(WHISPER_RELEASE_DIR);
        let model = tools.join(WHISPER_MODEL);
        let mut pins: Vec<Pin> =
            WHISPER_BINARIES.iter().map(|&(name, sha256)| Pin::new(dir.join(name), sha256)).collect();
        pins.push(Pin::new(&model, WHISPER_MODEL_SHA256));
        WhisperConfig { cli_path: dir.join("whisper-cli.exe"), model_path: model, pins }
    }
}

#[derive(Debug)]
pub enum TranscribeError {
    /// The CLI binary or model file isn't where `WhisperConfig` says.
    NotInstalled(PathBuf),
    /// A pinned file isn't the one that was vetted, so nothing was run.
    Integrity(integrity::IntegrityError),
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
            TranscribeError::Integrity(e) => write!(f, "{e}"),
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
    // Held until whisper-cli has exited: it loads its DLLs and reads the
    // model while it runs, and none of them can be swapped while these
    // handles are open.
    let _verified = integrity::verify_all(&config.pins).map_err(TranscribeError::Integrity)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A swapped DLL or model must stop transcription before whisper-cli is
    /// started — once it runs, whatever was swapped in already has too.
    #[test]
    fn a_file_that_fails_its_pin_stops_transcription_before_whisper_runs() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("whisper-cli.exe");
        let model = dir.path().join("ggml-base.en.bin");
        let dll = dir.path().join("whisper.dll");
        // All present, so the not-installed checks pass and the pin is what's
        // being tested.
        for path in [&cli, &model, &dll] {
            std::fs::write(path, b"not the vetted file").unwrap();
        }
        let config = WhisperConfig { cli_path: cli, model_path: model, pins: vec![Pin::new(&dll, WHISPER_MODEL_SHA256)] };

        let err = transcribe(&[0.0; 1600], 1, 16_000, &config).expect_err("a swapped DLL must be refused");

        assert!(matches!(err, TranscribeError::Integrity(_)), "expected an integrity refusal, got: {err}");
        assert!(err.to_string().contains("whisper.dll"), "should name the file: {err}");
    }

    #[test]
    fn the_default_config_pins_the_cli_and_the_model_it_runs() {
        let config = WhisperConfig::default();
        let pinned: Vec<&Path> = config.pins.iter().map(|pin| pin.path.as_path()).collect();

        assert!(pinned.contains(&config.cli_path.as_path()), "the CLI itself must be pinned");
        assert!(pinned.contains(&config.model_path.as_path()), "the model must be pinned");
    }
}
