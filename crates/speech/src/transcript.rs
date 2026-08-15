//! Parses whisper.cpp's `--output-json --output-json-full` format into a
//! plain `Transcript`, doing the two things the raw format needs before it's
//! usable as either captions or a clickable transcript panel: dropping
//! whisper's bracketed control tokens (`[_BEG_]`, `[_TT_123]`, ...) — not
//! speech, and would otherwise show up as a "word" — and merging bare
//! punctuation tokens onto the word before them, since whisper tokenises
//! "everybody," as two separate tokens and a transcript panel where half the
//! clickable words are stray commas would be unreadable and un-clickable in
//! the wrong places.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    pub start_ms: u32,
    pub end_ms: u32,
    pub words: Vec<Word>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Transcript {
    pub segments: Vec<Segment>,
}

#[derive(Debug)]
pub enum ParseError {
    InvalidJson(serde_json::Error),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::InvalidJson(e) => write!(f, "invalid whisper JSON output: {e}"),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Deserialize)]
struct RawRoot {
    transcription: Vec<RawSegment>,
}

#[derive(Deserialize)]
struct RawSegment {
    offsets: RawOffsets,
    text: String,
    #[serde(default)]
    tokens: Vec<RawToken>,
}

#[derive(Deserialize)]
struct RawOffsets {
    from: u32,
    to: u32,
}

#[derive(Deserialize)]
struct RawToken {
    text: String,
    offsets: RawOffsets,
}

/// A whisper control token: wrapped in brackets end to end, never real
/// transcribed speech. Checked structurally (starts with `[`, ends with `]`)
/// rather than against a fixed list, since whisper's token vocabulary
/// includes many of these (`[_BEG_]`, `[_TT_123]`, `[_SOT_]`, ...) and a
/// fixed list would silently miss ones this code hasn't seen yet.
fn is_control_token(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with('[') && trimmed.ends_with(']')
}

/// A token that's pure punctuation/whitespace with no letters or digits of
/// its own — whisper emits these as separate tokens from the word they
/// visually attach to (`"everybody"` then `","`), so they merge onto
/// whatever word came before rather than becoming their own "word".
fn is_bare_punctuation(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty() && !trimmed.chars().any(|c| c.is_alphanumeric())
}

fn words_from_tokens(tokens: &[RawToken]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    for token in tokens {
        if is_control_token(&token.text) {
            continue;
        }
        let trimmed = token.text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_bare_punctuation(trimmed) {
            if let Some(last) = words.last_mut() {
                last.text.push_str(trimmed);
                last.end_ms = token.offsets.to;
            }
            // A punctuation token with no preceding word (shouldn't happen in
            // practice — whisper doesn't open a segment with a comma — but
            // dropping it rather than panicking is the safe response to
            // output this code doesn't control the shape of).
            continue;
        }
        words.push(Word { text: trimmed.to_string(), start_ms: token.offsets.from, end_ms: token.offsets.to });
    }
    words
}

/// Parses whisper.cpp's `--output-json-full` JSON text into a `Transcript`.
pub fn parse_whisper_json(json: &str) -> Result<Transcript, ParseError> {
    let raw: RawRoot = serde_json::from_str(json).map_err(ParseError::InvalidJson)?;
    let segments = raw
        .transcription
        .into_iter()
        .map(|s| Segment {
            text: s.text.trim().to_string(),
            start_ms: s.offsets.from,
            end_ms: s.offsets.to,
            words: words_from_tokens(&s.tokens),
        })
        .collect();
    Ok(Transcript { segments })
}
