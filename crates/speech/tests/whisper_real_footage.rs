//! End-to-end smoke test against the real prebuilt `whisper-cli.exe` and a
//! real `.bin` model on this machine — the integration risk the pure JSON-
//! parsing tests in `transcript.rs` can't reach: WAV writing, subprocess
//! invocation, and reading the CLI's actual output back off disk.
//!
//! Env-gated and `#[ignore]`d, same convention as every other real-footage
//! test in this project: machine-specific paths and a large model file
//! aren't something to assume present in an ordinary `cargo test` run.
//!
//! ```text
//! NLE_REAL_FOOTAGE_PATH="C:/path/to/real_recording.mp4" \
//!   cargo test -p speech --test whisper_real_footage -- --ignored --nocapture
//! ```

use speech::{transcribe, WhisperConfig};

#[test]
#[ignore]
fn transcribes_real_speech_from_a_real_recording() {
    let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
        Ok(p) => p,
        Err(_) => panic!("set NLE_REAL_FOOTAGE_PATH to a real video/audio file and run with --ignored"),
    };
    let config = WhisperConfig::default();
    assert!(
        config.cli_path.is_file(),
        "whisper-cli.exe not found at {} — see docs/decisions-log.md's speech entry for setup",
        config.cli_path.display()
    );
    assert!(config.model_path.is_file(), "model not found at {}", config.model_path.display());

    // Decode a real chunk of audio directly via ffmpeg CLI into raw PCM, to
    // keep this crate's own test independent of `media_ffmpeg`/`audio_source`
    // (this crate doesn't depend on either — it operates on samples handed
    // to it, and this test hands it real ones extracted the simplest way
    // available in a test context).
    let wav_out = std::env::temp_dir().join("nle_speech_smoke_input.wav");
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-i", &path, "-t", "15", "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
        .arg(&wav_out)
        .status()
        .expect("ffmpeg must be on PATH to prepare this test's input audio");
    assert!(status.success(), "ffmpeg failed to extract test audio");

    let samples = read_wav_i16_as_f32(&wav_out);
    let transcript = transcribe(&samples, 1, 16000, &config).expect("transcription should succeed");

    println!("real footage transcript: {} segment(s)", transcript.segments.len());
    for seg in &transcript.segments {
        println!("  [{:>6}-{:<6}] {}", seg.start_ms, seg.end_ms, seg.text);
    }

    assert!(!transcript.segments.is_empty(), "expected at least one segment of real speech");
    let total_words: usize = transcript.segments.iter().map(|s| s.words.len()).sum();
    assert!(total_words > 0, "expected at least some words with timestamps");
    for seg in &transcript.segments {
        assert!(!seg.text.trim().is_empty(), "a segment's text should not be blank");
        for w in &seg.words {
            assert!(w.end_ms >= w.start_ms, "a word's end must not precede its start: {w:?}");
            assert!(!w.text.trim().is_empty(), "a word should not be blank");
        }
    }
}

/// Minimal 16-bit PCM mono WAV reader — just enough to read back the fixed
/// format this test's own `ffmpeg` invocation produces, not a general parser.
fn read_wav_i16_as_f32(path: &std::path::Path) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("failed to read generated WAV");
    let data_pos = bytes.windows(4).position(|w| w == b"data").expect("no data chunk") + 8;
    bytes[data_pos..]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / i16::MAX as f32)
        .collect()
}
