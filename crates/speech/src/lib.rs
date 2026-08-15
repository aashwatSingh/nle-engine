//! Local speech-to-text: shells out to a prebuilt `whisper.cpp` CLI rather
//! than linking whisper.cpp in-process (see `docs/decisions-log.md` for why —
//! short version: the from-source FFI build fought this machine's mixed
//! MinGW/MSYS/Cygwin shell environment hard enough, in a way deep enough in
//! CMake's own compiler-detection internals, that a subprocess around an
//! official prebuilt binary is the more honest engineering choice, not a
//! shortcut). Still fully local: no network call happens at transcription
//! time, only at the one-time setup of downloading the binary and model.

pub mod transcript;
pub mod whisper;

pub use transcript::{parse_whisper_json, ParseError, Segment, Transcript, Word};
pub use whisper::{transcribe, TranscribeError, WhisperConfig};
