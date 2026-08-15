//! Export smoke test against a **real** video file — see
//! `crates/playback/tests/real_footage_smoke.rs` for why this exists: every
//! other export test in this workspace renders synthetic ffmpeg test patterns,
//! which says nothing about whether the export pipeline holds up against real
//! camera or screen-capture footage's actual codec, GOP structure, and audio.
//!
//! Ignored by default, gated on `NLE_REAL_FOOTAGE_PATH` for the same reasons
//! as the playback version. Run with:
//!
//! ```text
//! NLE_REAL_FOOTAGE_PATH="C:/Users/you/Videos/some_recording.mp4" \
//!   cargo test -p export --test real_footage_smoke -- --ignored --nocapture
//! ```

use export::{export_sequence, ExportOptions};
use std::collections::HashMap;
use std::path::PathBuf;
use timeline::{
    ClipInstance, ClipInstanceId, ClipSource, ParamValue, Project, Sequence, SequenceId,
    SequenceSettings, SpeedCurve, TimeTick, Track, TrackId, TrackKind, TIMEBASE,
};

const SEQ: SequenceId = SequenceId(1);

fn real_footage_path() -> Option<PathBuf> {
    std::env::var("NLE_REAL_FOOTAGE_PATH").ok().map(PathBuf::from)
}

/// Mirrors `app::state::standard_frame_rate` — see the playback smoke test's
/// copy of this for why it's duplicated rather than shared.
fn standard_frame_rate(kind: &media::FrameRateKind) -> timeline::FrameRate {
    let media::FrameRateKind::Constant(r) = kind else {
        panic!("expected a constant-frame-rate source, got {kind:?}");
    };
    [
        timeline::FrameRate::Fps23_976,
        timeline::FrameRate::Fps24,
        timeline::FrameRate::Fps25,
        timeline::FrameRate::Fps29_97,
        timeline::FrameRate::Fps30,
        timeline::FrameRate::Fps50,
        timeline::FrameRate::Fps59_94,
        timeline::FrameRate::Fps60,
    ]
    .into_iter()
    .find(|std| {
        let (n, d) = std.as_rational();
        n as u64 * r.den as u64 == d as u64 * r.num as u64
    })
    .unwrap_or_else(|| panic!("real footage's frame rate {r:?} isn't one of the standard rates"))
}

#[test]
#[ignore]
fn exports_a_short_real_clip_at_native_resolution_with_correct_duration() {
    let path = real_footage_path()
        .expect("set NLE_REAL_FOOTAGE_PATH and run with --ignored to use this test");
    assert!(path.exists(), "NLE_REAL_FOOTAGE_PATH does not point to a real file: {path:?}");

    media_ffmpeg::init().unwrap();
    let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
    let video = asset.video.clone().expect("expected a video stream");
    let has_audio = asset.audio.is_some();
    println!("exporting real footage: {}x{}, audio: {has_audio}", video.width, video.height);

    let duration_ticks = TIMEBASE * 5; // 5s of real footage
    assert!((asset.duration_ticks as i64) >= duration_ticks, "footage shorter than the test window");

    let clip = ClipInstance {
        id: ClipInstanceId(1),
        source: ClipSource::Media(asset.id),
        source_in: TimeTick(0),
        source_out: TimeTick(duration_ticks),
        timeline_in: TimeTick(0),
        timeline_out: TimeTick(duration_ticks),
        speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
        effects: vec![],
        audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
        audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
        linked_group: None,
    };
    let mut tracks = vec![Track {
        id: TrackId(1),
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![clip.clone()],
        transitions: vec![],
        gain_db: timeline::unity_gain(),
        pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    }];
    if has_audio {
        tracks.push(Track {
            id: TrackId(2),
            kind: TrackKind::Audio,
            name: "A1".into(),
            clips: vec![ClipInstance { id: ClipInstanceId(2), ..clip }],
            transitions: vec![],
            gain_db: timeline::unity_gain(),
            pan: 0.0,
            locked: false,
            sync_locked: true,
            muted: false,
            solo: false,
            height_px: 60,
        });
    }
    let frame_rate = standard_frame_rate(&video.frame_rate);
    let (width, height) = (video.width, video.height);
    let project = Project {
        sequences: vec![Sequence {
            id: SEQ,
            name: "S".into(),
            settings: SequenceSettings {
                frame_rate,
                width,
                height,
                sample_rate: 48_000,
                working_color_primaries: video.color.primaries,
                drop_frame_timecode: false,
            },
            tracks,
            markers: vec![],
        }],
        assets: vec![asset],
        bins: vec![],
    };
    let mut paths = HashMap::new();
    paths.insert(project.assets[0].id, path);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("real_footage_export.mp4");
    let stats = export_sequence(&project, SEQ, &paths, &out, &ExportOptions::default(), |_, _| true)
        .expect("export of real footage should succeed");

    println!(
        "exported {} frames ({}x{}), {} missing-source frames, audio written: {}",
        stats.frames_written, stats.width, stats.height, stats.frames_with_missing_sources, stats.audio_written
    );
    assert_eq!(
        stats.frames_with_missing_sources, 0,
        "every frame of real footage should have decoded — this is the case export's persistent \
         SourceReader exists for"
    );
    let expected_frames = (duration_ticks / frame_rate.ticks_per_frame()) as u32;
    assert_eq!(stats.frames_written, expected_frames);

    // Verify the file on disk, not just the in-process stats — this is what
    // actually matters: a real player must be able to open what was written.
    let exported = media_ffmpeg::probe(&out).expect("the exported file should itself be probeable");
    assert_eq!((exported.video.unwrap().width, height), (width, height));
    let exported_seconds = exported.duration_ticks as f64 / TIMEBASE as f64;
    let expected_seconds = duration_ticks as f64 / TIMEBASE as f64;
    assert!(
        (exported_seconds - expected_seconds).abs() < 0.2,
        "exported duration {exported_seconds:.2}s should match the requested {expected_seconds:.2}s"
    );
    if has_audio {
        assert!(stats.audio_written, "the source had audio, the export should too");
    }
}
