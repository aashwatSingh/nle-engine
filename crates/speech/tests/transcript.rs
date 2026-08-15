//! Parsing whisper.cpp's `--output-json-full` format into `speech::Transcript`.
//!
//! The fixtures here are trimmed, real output — captured from an actual
//! `whisper-cli -oj -ojf` run against a real recording (see
//! `docs/decisions-log.md`'s speech entry), not hand-guessed at the shape of
//! the format. Whisper's own token stream includes special tokens
//! (`[_BEG_]`, `[_TT_123]`, etc.) mixed in with real words; a naive "every
//! token is a word" parse would put `[_BEG_]` into the transcript panel as
//! if a speaker said it.

use speech::{parse_whisper_json, ParseError};

const REAL_FIXTURE: &str = r#"{
	"systeminfo": "WHISPER : COREML = 0 | OPENVINO = 0",
	"model": { "type": "base", "multilingual": false },
	"params": { "model": "ggml-base.en.bin", "language": "en", "translate": false },
	"result": { "language": "en" },
	"transcription": [
		{
			"timestamps": { "from": "00:00:00,000", "to": "00:00:06,240" },
			"offsets": { "from": 0, "to": 6240 },
			"text": " Hello everybody, welcome. Let's talk about something we all feel, but rarely say out loud.",
			"tokens": [
				{ "text": "[_BEG_]", "timestamps": { "from": "00:00:00,000", "to": "00:00:00,000" }, "offsets": { "from": 0, "to": 0 }, "id": 50363, "p": 0.914895, "t_dtw": -1 },
				{ "text": " Hello", "timestamps": { "from": "00:00:00,360", "to": "00:00:00,430" }, "offsets": { "from": 360, "to": 430 }, "id": 18435, "p": 0.811479, "t_dtw": -1 },
				{ "text": " everybody", "timestamps": { "from": "00:00:00,430", "to": "00:00:01,200" }, "offsets": { "from": 430, "to": 1200 }, "id": 7288, "p": 0.784304, "t_dtw": -1 },
				{ "text": ",", "timestamps": { "from": "00:00:01,210", "to": "00:00:01,380" }, "offsets": { "from": 1210, "to": 1380 }, "id": 11, "p": 0.508538, "t_dtw": -1 },
				{ "text": " welcome", "timestamps": { "from": "00:00:01,380", "to": "00:00:01,850" }, "offsets": { "from": 1380, "to": 1850 }, "id": 7062, "p": 0.948503, "t_dtw": -1 },
				{ "text": ".", "timestamps": { "from": "00:00:02,170", "to": "00:00:02,170" }, "offsets": { "from": 2170, "to": 2170 }, "id": 13, "p": 0.615501, "t_dtw": -1 }
			]
		},
		{
			"timestamps": { "from": "00:00:06,240", "to": "00:00:14,120" },
			"offsets": { "from": 6240, "to": 14120 },
			"text": " Social media is broken.",
			"tokens": [
				{ "text": "[_BEG_]", "timestamps": { "from": "00:00:06,240", "to": "00:00:06,240" }, "offsets": { "from": 6240, "to": 6240 }, "id": 50363, "p": 0.9, "t_dtw": -1 },
				{ "text": " Social", "timestamps": { "from": "00:00:06,300", "to": "00:00:06,600" }, "offsets": { "from": 6300, "to": 6600 }, "id": 1, "p": 0.9, "t_dtw": -1 },
				{ "text": " media", "timestamps": { "from": "00:00:06,600", "to": "00:00:06,900" }, "offsets": { "from": 6600, "to": 6900 }, "id": 2, "p": 0.9, "t_dtw": -1 }
			]
		}
	]
}"#;

#[test]
fn parses_every_segment_with_its_text_and_timing() {
    let transcript = parse_whisper_json(REAL_FIXTURE).expect("real whisper output should parse");
    assert_eq!(transcript.segments.len(), 2);
    assert_eq!(transcript.segments[0].start_ms, 0);
    assert_eq!(transcript.segments[0].end_ms, 6240);
    assert_eq!(
        transcript.segments[0].text,
        "Hello everybody, welcome. Let's talk about something we all feel, but rarely say out loud."
    );
    assert_eq!(transcript.segments[1].start_ms, 6240);
    assert_eq!(transcript.segments[1].end_ms, 14120);
}

#[test]
fn special_tokens_are_excluded_from_words() {
    // `[_BEG_]` (and whisper's other bracketed control tokens) are not
    // something anyone said — including it as a "word" would put it in the
    // transcript panel and let it get spoken-position-clicked like real text.
    let transcript = parse_whisper_json(REAL_FIXTURE).unwrap();
    let words = &transcript.segments[0].words;
    assert!(
        words.iter().all(|w| !w.text.starts_with('[')),
        "no word should be a bracketed control token, got {words:?}"
    );
}

#[test]
fn punctuation_tokens_attach_to_the_preceding_word_rather_than_standing_alone() {
    // Whisper tokenises "everybody," as two tokens: " everybody" then ",".
    // Treating "," as its own clickable word would be a transcript panel
    // where half the "words" are stray punctuation marks. It should merge
    // into "everybody," instead — reading naturally and click-to-seek not
    // stopping short of the sound the punctuation is anchored to.
    let transcript = parse_whisper_json(REAL_FIXTURE).unwrap();
    let words = &transcript.segments[0].words;
    let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
    assert!(texts.contains(&"everybody,"), "expected merged \"everybody,\", got {texts:?}");
    assert!(!texts.contains(&","), "a bare comma should never be its own word, got {texts:?}");
}

#[test]
fn word_timestamps_come_from_the_underlying_tokens_in_milliseconds() {
    let transcript = parse_whisper_json(REAL_FIXTURE).unwrap();
    let hello = transcript.segments[0].words.iter().find(|w| w.text == "Hello").expect("Hello should be a word");
    assert_eq!(hello.start_ms, 360);
    assert_eq!(hello.end_ms, 430);
}

#[test]
fn words_are_in_chronological_order_within_a_segment() {
    let transcript = parse_whisper_json(REAL_FIXTURE).unwrap();
    let words = &transcript.segments[0].words;
    for pair in words.windows(2) {
        assert!(pair[0].start_ms <= pair[1].start_ms, "words out of order: {:?} then {:?}", pair[0], pair[1]);
    }
}

#[test]
fn a_transcript_with_no_segments_parses_to_an_empty_transcript() {
    let empty = r#"{"transcription": []}"#;
    let transcript = parse_whisper_json(empty).unwrap();
    assert!(transcript.segments.is_empty());
}

#[test]
fn malformed_json_is_a_clear_error_not_a_panic() {
    let result = parse_whisper_json("not valid json at all {{{");
    assert!(matches!(result, Err(ParseError::InvalidJson(_))));
}

#[test]
fn json_missing_the_transcription_key_is_a_clear_error() {
    let result = parse_whisper_json(r#"{"model": {"type": "base"}}"#);
    assert!(result.is_err(), "missing the top-level `transcription` array should be a real error, not an empty transcript");
}
