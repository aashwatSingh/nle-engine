//! Smoke test against a **real** video file — not the synthetic ffmpeg test
//! patterns every other test in this workspace uses.
//!
//! Every existing measurement of playback throughput (the "27x faster than
//! reopen-per-frame" decode benchmark, the "holds frame rate" acceptance test)
//! was taken against `test_playback_demo.mp4`: a 640x360 ffmpeg-generated color
//! bar pattern. That's a fine, fast, deterministic fixture for testing
//! *correctness* — but it says nothing about whether the pipeline holds up
//! against real camera or screen-capture footage at a real delivery
//! resolution, with real inter-frame compression and real audio. This test
//! exists to close exactly that gap, on demand, on a machine that has such a
//! file.
//!
//! Ignored by default and gated on an environment variable rather than a
//! bundled fixture: real footage is large (hundreds of MB), machine-specific,
//! and not something to commit into the repo or run in ordinary `cargo test`.
//! Run explicitly with:
//!
//! ```text
//! NLE_REAL_FOOTAGE_PATH="C:/Users/you/Videos/some_recording.mp4" \
//!   cargo test -p playback --test real_footage_smoke -- --ignored --nocapture
//! ```

use playback::{SequenceAudioEngine, SequenceVideoPlayback};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use timeline::{
    ClipInstance, ClipInstanceId, ClipSource, ParamValue, Project, Sequence, SequenceId,
    SequenceSettings, SpeedCurve, TimeTick, Track, TrackId, TrackKind, TIMEBASE,
};

const SEQ: SequenceId = SequenceId(1);

fn real_footage_path() -> Option<PathBuf> {
    std::env::var("NLE_REAL_FOOTAGE_PATH").ok().map(PathBuf::from)
}

/// Maps a probed source rate onto one of `timeline::FrameRate`'s named
/// constants. Mirrors `app::state::standard_frame_rate` — real footage is
/// virtually always one of these, and duplicating the match here avoids
/// pulling the whole `app` crate (with its GPU/windowing dependencies) into
/// `playback`'s test-only dependency graph for one small function.
fn standard_frame_rate(kind: &media::FrameRateKind) -> timeline::FrameRate {
    let media::FrameRateKind::Constant(r) = kind else {
        panic!("real footage smoke test expects a constant-frame-rate source, got {kind:?}");
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

/// A one-clip sequence covering the first `duration_ticks` of the real file,
/// at the file's own probed resolution and frame rate — not a resized or
/// re-timed stand-in.
fn setup(duration_ticks: i64) -> (Arc<Project>, HashMap<media::MediaAssetId, PathBuf>, bool) {
    let path = real_footage_path().expect(
        "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test",
    );
    assert!(path.exists(), "NLE_REAL_FOOTAGE_PATH does not point to a real file: {path:?}");

    media_ffmpeg::init().unwrap();
    let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
    let video = asset.video.as_ref().expect("expected a video stream in the real footage");
    let has_audio = asset.audio.is_some();

    println!(
        "real footage: {}x{} @ {:?}, {:.1}s, audio: {}",
        video.width,
        video.height,
        video.frame_rate,
        asset.duration_ticks as f64 / TIMEBASE as f64,
        has_audio
    );
    assert!(
        (asset.duration_ticks as i64) >= duration_ticks,
        "test asks for more of the file than it contains — pick a longer recording or a shorter window"
    );

    let mut paths = HashMap::new();
    paths.insert(asset.id, path);

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

    let project = Project {
        sequences: vec![Sequence {
            id: SEQ,
            name: "S".into(),
            settings: SequenceSettings {
                frame_rate: standard_frame_rate(&video.frame_rate),
                width: video.width,
                height: video.height,
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
    (Arc::new(project), paths, has_audio)
}

/// Shared measurement behind both variants below, so the CPU and GPU numbers
/// come from the exact same methodology and are directly comparable — that's
/// the whole point of running both against the same file.
fn measure_video_throughput(gpu: Option<playback::GpuContext>, label: &str) {
    let (project, paths, _has_audio) = setup(TIMEBASE * 8);
    let clock = Arc::new(AtomicI64::new(0));
    let clock_for_engine = clock.clone();
    let video = SequenceVideoPlayback::start(
        project,
        SEQ,
        paths,
        0,
        move || clock_for_engine.load(Ordering::Relaxed),
        gpu,
    );

    let window = Duration::from_secs(4);
    let started = Instant::now();
    let mut delivered = 0u32;
    let mut polls = 0u32;
    while started.elapsed() < window {
        let now_ticks = (started.elapsed().as_secs_f64() * TIMEBASE as f64) as i64;
        clock.store(now_ticks, Ordering::Relaxed);
        polls += 1;
        if video.frame_for(now_ticks).is_some() {
            delivered += 1;
        }
        std::thread::sleep(Duration::from_millis(4));
    }

    let fps = 30.0;
    let boundaries_crossed = (window.as_secs_f64() * fps) as u32;
    println!(
        "[{label}] real footage decode: {delivered}/{polls} polls returned a frame, \
         ~{boundaries_crossed} frame boundaries crossed, {} dropped, {} starved",
        video.dropped_frame_count(),
        video.starved_count()
    );

    assert!(
        delivered >= boundaries_crossed * 60 / 100,
        "[{label}] delivered a frame for only {delivered} of ~{boundaries_crossed} boundaries on \
         real footage — the decode pipeline is not holding up at this resolution/codec"
    );
}

#[test]
#[ignore]
fn video_holds_frame_rate_on_real_footage() {
    // Same methodology as sequence_video_realtime.rs's synthetic-fixture
    // version — deliberately, so the two numbers are comparable. The
    // synthetic version is 640x360; this measures the same property at
    // whatever resolution the real file actually is. `None` is the CPU-buffer
    // path — what the whole editor used before GPU-resident frames existed,
    // and still what a caller with no GPU context gets.
    measure_video_throughput(None, "cpu");
}

#[test]
#[ignore]
fn video_holds_frame_rate_on_real_footage_with_gpu_upload() {
    // The other half of the same measurement, with a GPU context — the path
    // the running app actually takes now. Run both variants back to back
    // against the same file to see the real before/after, not an assumption:
    //
    //   cargo test -p playback --test real_footage_smoke -- --ignored \
    //     --nocapture --test-threads=1 video_holds_frame_rate
    let Some((device, queue)) = render::headless_context() else {
        panic!("no GPU adapter — this project cannot run without one, so this is a real failure");
    };
    measure_video_throughput(Some((device, queue)), "gpu");
}

#[test]
#[ignore]
fn audio_plays_at_real_time_rate_on_real_footage() {
    let (project, paths, has_audio) = setup(TIMEBASE * 8);
    if !has_audio {
        println!("real footage has no audio track — skipping");
        return;
    }
    let engine = SequenceAudioEngine::start(project, SEQ, paths, 0, 0.0)
        .expect("needs a default audio output device");

    std::thread::sleep(Duration::from_millis(500));
    let a = engine.current_tick();
    std::thread::sleep(Duration::from_millis(1500));
    let b = engine.current_tick();

    let advanced = (b - a) as f64 / TIMEBASE as f64;
    println!(
        "real footage audio: {advanced:.3}s advanced over 1.5s wall time, {} underruns",
        engine.underrun_count()
    );
    assert!(
        (1.1..1.9).contains(&advanced),
        "audio clock should advance at roughly real-time rate on real footage, got {advanced:.3}s"
    );
    assert_eq!(engine.underrun_count(), 0, "the mixer should keep up with real footage's own audio");
}
