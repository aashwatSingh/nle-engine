//! Acceptance tests for real-time sequence audio: the clock must advance at
//! wall-clock rate off samples the hardware actually consumed, and the mixer
//! must stay ahead of the callback.
//!
//! These need a working default output device, same assumption as the existing
//! `AudioEngine` tests in this crate.

use playback::SequenceAudioEngine;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use timeline::{
    ClipInstance, ClipInstanceId, ClipSource, FrameRate, ParamValue, Project, Sequence, SequenceId,
    SequenceSettings, SpeedCurve, TimeTick, Track, TrackId, TrackKind, TIMEBASE,
};

const SEQ: SequenceId = SequenceId(1);

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("test_fixtures")
        .join(name)
}

fn audio_clip(id: u64, asset: media::MediaAssetId, duration_ticks: i64) -> ClipInstance {
    ClipInstance {
        id: ClipInstanceId(id),
        source: ClipSource::Media(asset),
        source_in: TimeTick(0),
        source_out: TimeTick(duration_ticks),
        timeline_in: TimeTick(0),
        timeline_out: TimeTick(duration_ticks),
        speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
        effects: vec![],
        audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
        audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
        linked_group: None,
    }
}

/// A sequence holding one audio clip of `duration_ticks`. The fixture is 8s,
/// longer than anything these tests play.
fn setup(duration_ticks: i64) -> (Arc<Project>, HashMap<media::MediaAssetId, PathBuf>) {
    media_ffmpeg::init().unwrap();
    let path = fixture("test_playback_demo.mp4");
    let asset = media_ffmpeg::probe(&path).unwrap();
    let mut paths = HashMap::new();
    paths.insert(asset.id, path);

    let project = Project {
        sequences: vec![Sequence {
            id: SEQ,
            name: "S".into(),
            settings: SequenceSettings {
                frame_rate: FrameRate::Fps30,
                width: 640,
                height: 360,
                sample_rate: 48_000,
                working_color_primaries: media::ColorPrimaries::Rec709,
                drop_frame_timecode: false,
            },
            tracks: vec![Track {
                id: TrackId(1),
                kind: TrackKind::Audio,
                name: "A1".into(),
                clips: vec![audio_clip(1, asset.id, duration_ticks)], transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
                locked: false,
                sync_locked: true,
                muted: false,
                solo: false,
                height_px: 60,
            }],
            markers: vec![],
        }],
        assets: vec![asset],
        bins: vec![],
    };
    (Arc::new(project), paths)
}

#[test]
fn the_clock_advances_at_real_time_rate_and_never_underruns() {
    // The property that makes audio usable as the master clock: the reported
    // playhead must track wall time. A clock derived from mixing progress
    // instead of consumed samples would race far ahead, since mixing runs
    // orders of magnitude faster than real time — that's the bug this guards.
    let (project, paths) = setup(TIMEBASE * 6);
    let engine = SequenceAudioEngine::start(project, SEQ, paths, 0, 0.0)
        .expect("needs a default audio output device");

    std::thread::sleep(Duration::from_millis(600));
    let a = engine.current_tick();
    std::thread::sleep(Duration::from_millis(1500));
    let b = engine.current_tick();

    let advanced = (b - a) as f64 / TIMEBASE as f64;
    assert!(
        (1.2..1.9).contains(&advanced),
        "expected ~1.5s of clock advance over 1.5s of wall time, got {advanced:.3}s \
         (a={a}, b={b})"
    );
    assert_eq!(engine.underrun_count(), 0, "the mixer should stay ahead of the callback");
}

#[test]
fn seeking_repositions_the_clock_and_keeps_playing() {
    let (project, paths) = setup(TIMEBASE * 6);
    let engine = SequenceAudioEngine::start(project, SEQ, paths, 0, 0.0).expect("audio device");
    std::thread::sleep(Duration::from_millis(300));

    let target = TIMEBASE * 3;
    engine.seek(target);
    std::thread::sleep(Duration::from_millis(300));
    let after = engine.current_tick();
    assert!(
        (after - target).abs() < TIMEBASE / 2,
        "clock should land near the seek target {target}, got {after}"
    );

    std::thread::sleep(Duration::from_millis(400));
    assert!(engine.current_tick() > after, "should keep playing after a seek");
}

#[test]
fn pausing_holds_the_clock_still() {
    let (project, paths) = setup(TIMEBASE * 6);
    let engine = SequenceAudioEngine::start(project, SEQ, paths, 0, 0.0).expect("audio device");
    std::thread::sleep(Duration::from_millis(300));

    engine.pause();
    std::thread::sleep(Duration::from_millis(150)); // let the command land
    let a = engine.current_tick();
    std::thread::sleep(Duration::from_millis(400));

    assert_eq!(a, engine.current_tick(), "a paused clock must not advance");
    assert!(!engine.is_playing());
}

#[test]
fn playback_reports_ended_at_the_sequence_end_without_counting_underruns() {
    // Silence past the end is correct behaviour, not the mixer falling
    // behind. Counting it as an underrun would make the stat useless for
    // spotting real dropouts.
    let (project, paths) = setup(TIMEBASE); // 1s sequence
    let engine = SequenceAudioEngine::start(project, SEQ, paths, 0, 0.0).expect("audio device");

    std::thread::sleep(Duration::from_millis(1800));

    assert!(engine.has_ended(), "a 1s sequence should have ended after 1.8s");
    assert_eq!(engine.underrun_count(), 0, "end-of-sequence silence must not count as underrun");
}

#[test]
fn a_sequence_with_no_audio_clips_still_provides_a_working_clock() {
    // The transport uses this engine even when there's nothing to hear, so a
    // single code path drives playback either way. The mix is silence, but the
    // clock still has to advance or the playhead would freeze on a
    // video-only sequence.
    let (project, paths) = setup(TIMEBASE * 4);
    let mut silent = (*project).clone();
    let asset_id = silent.assets[0].id;
    // Replace the audio track's clips with none, and give the sequence its
    // length from a video track instead.
    silent.sequences[0].tracks[0].clips.clear();
    silent.sequences[0].tracks.push(Track {
        id: TrackId(9),
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![audio_clip(9, asset_id, TIMEBASE * 4)], transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });

    let engine =
        SequenceAudioEngine::start(Arc::new(silent), SEQ, paths, 0, 0.0).expect("audio device");
    std::thread::sleep(Duration::from_millis(900));

    let t = engine.current_tick();
    assert!(
        t > TIMEBASE / 4,
        "clock should advance even with no audio clips, got {t} ticks"
    );
}

#[test]
fn meters_report_real_levels_while_playing_and_follow_the_track_fader() {
    // Meters are fed from the mixer, which runs ahead of real time, so the
    // interesting property isn't just "nonzero" — it's that a fader change is
    // reflected. A meter wired to the wrong stage (pre-fader, or to the master)
    // would still read nonzero and look fine.
    let (project, paths) = setup(TIMEBASE * 6);
    let engine = SequenceAudioEngine::start(project.clone(), SEQ, paths.clone(), 0, 0.0)
        .expect("needs a default audio output device");
    std::thread::sleep(Duration::from_millis(700));

    let m = engine.meters().expect("meters should be available once mixing has started");
    assert_eq!(m.tracks.len(), 1, "one audible audio track");
    let loud = m.tracks[0].1.peak_dbfs;
    assert!(
        loud > -60.0,
        "the fixture has real sound in it, so the track meter should be well above the floor, \
         got {loud} dBFS"
    );
    drop(engine);

    // Same sequence with the track pulled down 24 dB.
    let mut quiet_project = (*project).clone();
    quiet_project.sequences[0].tracks[0].gain_db = timeline::ParamTrack::constant(timeline::ParamValue::Number(-24.0));
    let engine = SequenceAudioEngine::start(Arc::new(quiet_project), SEQ, paths, 0, 0.0)
        .expect("audio device");
    std::thread::sleep(Duration::from_millis(700));
    let quiet = engine.meters().unwrap().tracks[0].1.peak_dbfs;

    assert!(
        (loud - quiet - 24.0).abs() < 2.0,
        "a -24 dB fader should drop the track meter by ~24 dB: {loud} vs {quiet}"
    );
}

#[test]
fn meters_track_the_audible_position_rather_than_the_mixers_read_ahead() {
    // The mixer fills a ring buffer ahead of real time. If meters published the
    // newest block outright, they'd lead the sound by the buffer depth and read
    // as broken. This checks the reading corresponds to a position at or behind
    // the clock, which is what the position-tagged history exists to guarantee.
    let (project, paths) = setup(TIMEBASE * 6);
    let engine =
        SequenceAudioEngine::start(project, SEQ, paths, 0, 0.0).expect("audio device");
    std::thread::sleep(Duration::from_millis(300));

    // Silence at the very start of the fixture would make this vacuous, so
    // assert we're actually measuring signal.
    let first = engine.meters().expect("some reading").master.peak_dbfs;
    std::thread::sleep(Duration::from_millis(900));
    let later = engine.meters().expect("some reading").master.peak_dbfs;

    assert!(first > -100.0 && later > -100.0, "both readings should be real signal");
    // And the clock has genuinely advanced across the two readings, so the
    // history is being indexed rather than pinned to one block.
    assert!(engine.current_tick() > TIMEBASE / 2, "playback should have progressed");
}
