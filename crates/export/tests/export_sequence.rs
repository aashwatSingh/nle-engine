//! Acceptance tests for export: the output must be a real, probe-able video
//! with the sequence's dimensions and frame count — and, critically, its
//! pixels must match what the compositor produces for the same tick, so
//! "what you see in the preview is what lands in the file" is verified
//! rather than assumed.

use export::{export_sequence, ExportError, ExportOptions, QualityPreset};
use std::collections::HashMap;
use std::path::PathBuf;
use timeline::{
    ClipInstance, ClipInstanceId, ClipSource, FrameRate, ParamValue, Project, Sequence,
    SequenceId, SequenceSettings, SpeedCurve, TimeTick, Track, TrackId, TrackKind,
};

/// Peak absolute sample value in a file's audio, decoded back out. The blunt
/// instrument that answers "is there actually sound in there".
fn peak_amplitude(path: &std::path::Path, sample_rate: u32) -> f32 {
    let mut stream = media_ffmpeg::AudioDecoderStream::open(path, sample_rate, 2)
        .expect("output should have a decodable audio stream");
    let mut peak = 0.0f32;
    while let Some(chunk) = stream.next_samples().unwrap() {
        for s in chunk {
            peak = peak.max(s.abs());
        }
    }
    peak
}

/// Per-channel peaks, for checking pan actually reached the file.
fn channel_peaks(path: &std::path::Path, sample_rate: u32) -> (f32, f32) {
    let mut stream = media_ffmpeg::AudioDecoderStream::open(path, sample_rate, 2).unwrap();
    let (mut l, mut r) = (0.0f32, 0.0f32);
    while let Some(chunk) = stream.next_samples().unwrap() {
        for f in chunk.chunks_exact(2) {
            l = l.max(f[0].abs());
            r = r.max(f[1].abs());
        }
    }
    (l, r)
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("test_fixtures").join(name)
}

const SEQ: SequenceId = SequenceId(1);

/// A one-video-track sequence holding `duration_ticks` of `asset`.
fn project_with_clip(asset: media::MediaAsset, duration_ticks: i64) -> Project {
    let asset_id = asset.id;
    Project {
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
                kind: TrackKind::Video,
                name: "V1".into(),
                clips: vec![ClipInstance {
                    id: ClipInstanceId(1),
                    source: ClipSource::Media(asset_id),
                    source_in: TimeTick(0),
                    source_out: TimeTick(duration_ticks),
                    timeline_in: TimeTick(0),
                    timeline_out: TimeTick(duration_ticks),
                    speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
                    effects: vec![],
                    audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                    audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                    linked_group: None,
                }], transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
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
    }
}

fn setup(duration_ticks: i64) -> (Project, HashMap<media::MediaAssetId, PathBuf>) {
    media_ffmpeg::init().unwrap();
    let path = fixture("test_playback_demo.mp4");
    let asset = media_ffmpeg::probe(&path).unwrap();
    let mut paths = HashMap::new();
    paths.insert(asset.id, path);
    (project_with_clip(asset, duration_ticks), paths)
}

/// Adds an audio track carrying the same asset over the same span, so the
/// sequence is a linked A/V edit like the editor produces.
fn add_audio_track(project: &mut Project, duration_ticks: i64) {
    let asset_id = project.assets[0].id;
    project.sequences[0].tracks.push(Track {
        id: TrackId(2),
        kind: TrackKind::Audio,
        name: "A1".into(),
        clips: vec![ClipInstance {
            id: ClipInstanceId(2),
            source: ClipSource::Media(asset_id),
            source_in: TimeTick(0),
            source_out: TimeTick(duration_ticks),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(duration_ticks),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }], transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
}

fn setup_with_audio(duration_ticks: i64) -> (Project, HashMap<media::MediaAssetId, PathBuf>) {
    let (mut project, paths) = setup(duration_ticks);
    add_audio_track(&mut project, duration_ticks);
    (project, paths)
}

#[test]
fn exports_a_real_playable_video_with_the_sequence_dimensions_and_length() {
    let seconds = 2;
    let duration = timeline::TIMEBASE * seconds;
    let (project, paths) = setup(duration);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.mp4");

    let mut progress_calls = 0;
    let stats = export_sequence(
        &project,
        SEQ,
        &paths,
        &out,
        &ExportOptions { quality: QualityPreset::Draft, ..Default::default() },
        |_, _| {
            progress_calls += 1;
            true
        },
    )
    .expect("export should succeed");

    assert_eq!(stats.frames_written, 30 * seconds as u32);
    assert_eq!(progress_calls, stats.frames_written, "progress reported once per frame");
    assert_eq!(
        stats.frames_with_missing_sources, 0,
        "every frame should have found its source"
    );

    // The real proof: ffmpeg can open what we wrote and it has the right shape.
    let probed = media_ffmpeg::probe(&out).expect("output must be a probe-able video");
    let video = probed.video.expect("output must have a video stream");
    assert_eq!((video.width, video.height), (640, 360));
    let probed_seconds = probed.duration_ticks as f64 / timeline::TIMEBASE as f64;
    assert!(
        (probed_seconds - seconds as f64).abs() < 0.2,
        "expected ~{seconds}s of output, probe reported {probed_seconds:.3}s"
    );
}

#[test]
fn exported_pixels_match_what_the_compositor_renders_for_the_same_tick() {
    // Guards the property the whole export path exists to deliver: the file
    // contains the composited frame, not a re-decode of the source, not an
    // off-by-one frame, and not a colour-space round trip that shifts the
    // image. Compares a decoded output frame against a direct compositor
    // render of the same tick.
    let duration = timeline::TIMEBASE; // 1s
    let (project, paths) = setup(duration);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.mp4");
    export_sequence(
        &project,
        SEQ,
        &paths,
        &out,
        &ExportOptions { quality: QualityPreset::Master, ..Default::default() },
        |_, _| true,
    )
    .unwrap();

    // Render tick 0 directly through the same compositor the preview uses.
    let (device, queue) = render::headless_context().expect("gpu");
    let compositor = render::Compositor::new(device, queue);
    let compiler = render::GraphCompiler::new(render::BuiltinRegistry::default());
    let graph = compiler.compile(&project, SEQ, TimeTick(0)).unwrap();
    let mut sources = render::SourceFrames::default();
    let asset = &project.assets[0];
    let src = media_ffmpeg::decode_frame_at(&paths[&asset.id], 0).unwrap();
    sources.insert(
        asset.id,
        src.pts_ticks,
        compositor.upload_rgba(&src.rgba, src.width, src.height, asset.video.as_ref().unwrap().color),
    );
    let (expected, _) = compositor.render_to_rgba(&graph, &sources, render::DeliverySpace::Rec709);

    let actual = media_ffmpeg::decode_frame_at(&out, 0).unwrap();
    assert_eq!((actual.width, actual.height), (expected.width, expected.height));

    // Compare mean absolute error per channel rather than exact equality:
    // even at CRF 0 the RGB->YUV420P->RGB round trip is lossy (chroma
    // subsampling alone guarantees it), so exact match would be testing
    // ffmpeg's colour conversion, not our pipeline. A low MAE proves it's
    // the same picture; a wrong frame or a double-applied transfer function
    // would land far above this.
    let n = expected.rgba.len().min(actual.rgba.len());
    let total: u64 = (0..n)
        .map(|i| (expected.rgba[i] as i32 - actual.rgba[i] as i32).unsigned_abs() as u64)
        .sum();
    let mae = total as f64 / n as f64;
    assert!(mae < 12.0, "exported frame differs from the composited frame (MAE {mae:.2}/255)");
}

#[test]
fn cancelling_stops_early_and_leaves_no_partial_file() {
    let (project, paths) = setup(timeline::TIMEBASE * 3);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cancelled.mp4");

    let result = export_sequence(&project, SEQ, &paths, &out, &ExportOptions::default(), |i, _| {
        i < 5 // cancel on the 6th frame
    });

    assert!(matches!(result, Err(ExportError::Cancelled)));
    assert!(!out.exists(), "a cancelled export must not leave a partial video behind");
}

#[test]
fn an_empty_sequence_is_rejected_rather_than_producing_an_empty_video() {
    media_ffmpeg::init().unwrap();
    let mut project = project_with_clip(
        media_ffmpeg::probe(&fixture("test_playback_demo.mp4")).unwrap(),
        timeline::TIMEBASE,
    );
    project.sequences[0].tracks.clear();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("empty.mp4");

    let result =
        export_sequence(&project, SEQ, &HashMap::new(), &out, &ExportOptions::default(), |_, _| true);

    assert!(matches!(result, Err(ExportError::EmptySequence)));
    assert!(!out.exists());
}

#[test]
fn missing_media_is_counted_not_silently_ignored() {
    // A project whose media can't be found still exports (as gaps), but the
    // caller must be able to tell — otherwise a black video looks like a
    // successful render.
    let (project, _) = setup(timeline::TIMEBASE);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("gaps.mp4");

    let stats = export_sequence(
        &project,
        SEQ,
        &HashMap::new(), // no paths: nothing resolvable
        &out,
        &ExportOptions { quality: QualityPreset::Draft, ..Default::default() },
        |_, _| true,
    )
    .expect("export should still complete");

    assert_eq!(stats.frames_written, 30);
    assert_eq!(stats.frames_with_missing_sources, 30, "every frame was missing its source");
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

#[test]
fn an_export_reports_the_loudness_and_true_peak_of_what_it_actually_wrote() {
    // Delivery specs are stated in LUFS and dBTP, so an export that can't
    // report them makes the user open the file in something else to find out
    // whether it's deliverable. Measured over the mixed output — the same
    // samples handed to the encoder — rather than estimated from the source
    // clips, since gain, pan and track faders all sit between the two.
    let seconds = 3;
    let duration = timeline::TIMEBASE * seconds;
    let (project, paths) = setup_with_audio(duration);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("loud.mp4");
    let opts = ExportOptions { quality: QualityPreset::Draft, ..Default::default() };

    let stats = export_sequence(&project, SEQ, &paths, &out, &opts, |_, _| true).unwrap();

    assert!(stats.audio_written);
    let loudness = stats.loudness.expect("an export with audio should report loudness");

    // The fixture is a real tone, so it must land in the range of plausible
    // programme loudness — not at the silence floor (which is what an
    // analyser that was never fed anything would report) and not above 0.
    assert!(
        loudness.integrated_lufs > -60.0 && loudness.integrated_lufs < 0.0,
        "implausible integrated loudness {} LUFS — the analyser probably never saw the mix",
        loudness.integrated_lufs
    );
    assert!(
        loudness.true_peak_dbtp > -60.0 && loudness.true_peak_dbtp < 6.0,
        "implausible true peak {} dBTP",
        loudness.true_peak_dbtp
    );
    // True peak is a peak and loudness is a gated average, so the peak is
    // necessarily the higher number. Getting this backwards would mean the
    // two fields are swapped — which no range check alone would catch.
    assert!(
        loudness.true_peak_dbtp > loudness.integrated_lufs,
        "true peak {} should exceed integrated loudness {}",
        loudness.true_peak_dbtp,
        loudness.integrated_lufs
    );
}

#[test]
fn an_export_with_no_audio_reports_no_loudness_rather_than_a_silence_reading() {
    // `Some(-120 LUFS)` would claim a measurement of a stream that was never
    // written; `None` says there was nothing to measure. A delivery check
    // reading the former would flag a silent-audio failure on a video-only
    // file that is perfectly correct.
    let (project, paths) = setup(timeline::TIMEBASE);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("silent.mp4");
    let opts = ExportOptions { quality: QualityPreset::Draft, ..Default::default() };

    let stats = export_sequence(&project, SEQ, &paths, &out, &opts, |_, _| true).unwrap();
    assert!(!stats.audio_written, "precondition: this fixture has no audio");
    assert!(stats.loudness.is_none(), "no audio stream means no loudness to report");
}

#[test]
fn exports_an_audio_stream_with_real_sound_in_it() {
    let seconds = 2;
    let duration = timeline::TIMEBASE * seconds;
    let (project, paths) = setup_with_audio(duration);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("av.mp4");
    let opts = ExportOptions { quality: QualityPreset::Draft, ..Default::default() };

    let stats = export_sequence(&project, SEQ, &paths, &out, &opts, |_, _| true).unwrap();

    assert!(stats.audio_written, "an audio track should have been written");
    assert_eq!(stats.audio_clips_with_no_source, 0);

    // The file must actually carry both streams.
    let probed = media_ffmpeg::probe(&out).unwrap();
    assert!(probed.video.is_some(), "output should still have video");
    let audio_info = probed.audio.expect("output should have an audio stream");
    assert_eq!(audio_info.channel_count, 2, "stereo out");
    assert_eq!(audio_info.sample_rate, opts.sample_rate);

    // And it must contain sound, not silence — the failure mode that a
    // stream-exists assertion alone would happily pass.
    let peak = peak_amplitude(&out, opts.sample_rate);
    assert!(peak > 0.01, "exported audio should not be silent (peak {peak})");

    // Audio length should track the video length rather than drifting.
    let audio_seconds = audio_info.duration_samples as f64 / audio_info.sample_rate as f64;
    assert!(
        (audio_seconds - seconds as f64).abs() < 0.25,
        "expected ~{seconds}s of audio, got {audio_seconds:.3}s"
    );
}

#[test]
fn a_sequence_with_no_audio_clips_gets_no_audio_stream() {
    // Rather than a silent track, which would be indistinguishable in the
    // file from a mix that failed.
    let (project, paths) = setup(timeline::TIMEBASE);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("silent.mp4");

    let stats = export_sequence(
        &project,
        SEQ,
        &paths,
        &out,
        &ExportOptions { quality: QualityPreset::Draft, ..Default::default() },
        |_, _| true,
    )
    .unwrap();

    assert!(!stats.audio_written);
    assert!(media_ffmpeg::probe(&out).unwrap().audio.is_none());
}

#[test]
fn muting_the_audio_track_produces_a_silent_but_present_stream() {
    // Mute is an explicit editorial choice, so the stream stays (the user
    // asked for a muted mix, not for no audio) but must carry no signal.
    let duration = timeline::TIMEBASE;
    let (mut project, paths) = setup_with_audio(duration);
    project.sequences[0].tracks[1].muted = true;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("muted.mp4");
    let opts = ExportOptions { quality: QualityPreset::Draft, ..Default::default() };

    let stats = export_sequence(&project, SEQ, &paths, &out, &opts, |_, _| true).unwrap();
    assert!(stats.audio_written);

    let peak = peak_amplitude(&out, opts.sample_rate);
    assert!(peak < 0.01, "a muted track should export near-silence, got peak {peak}");
}

#[test]
fn clip_gain_and_pan_survive_all_the_way_into_the_file() {
    // End-to-end proof the mix graph's staging isn't lost somewhere between
    // the mixer and the muxer: hard-right pan must show up as a channel
    // imbalance in the decoded output.
    let duration = timeline::TIMEBASE;
    let (mut project, paths) = setup_with_audio(duration);
    let clip = &mut project.sequences[0].tracks[1].clips[0];
    clip.audio_pan = timeline::ParamTrack::constant(ParamValue::Number(1.0));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("panned.mp4");
    let opts = ExportOptions { quality: QualityPreset::Draft, ..Default::default() };

    export_sequence(&project, SEQ, &paths, &out, &opts, |_, _| true).unwrap();

    let (l, r) = channel_peaks(&out, opts.sample_rate);
    assert!(r > 0.01, "the right channel should carry the signal (peak {r})");
    assert!(
        l < r * 0.35,
        "hard-right pan should leave the left channel much quieter (l {l}, r {r})"
    );
}

#[test]
fn a_gain_reduced_clip_exports_quieter_than_the_same_clip_at_unity() {
    // Two exports of the same edit differing only by clip gain: the -12dB one
    // must be measurably quieter. Catches a gain stage that's computed but
    // never applied.
    let duration = timeline::TIMEBASE;
    let dir = tempfile::tempdir().unwrap();
    let opts = ExportOptions { quality: QualityPreset::Draft, ..Default::default() };

    let (loud_project, paths) = setup_with_audio(duration);
    let loud = dir.path().join("loud.mp4");
    export_sequence(&loud_project, SEQ, &paths, &loud, &opts, |_, _| true).unwrap();

    let (mut quiet_project, _) = setup_with_audio(duration);
    quiet_project.sequences[0].tracks[1].clips[0].audio_gain_db =
        timeline::ParamTrack::constant(ParamValue::Number(-12.0));
    let quiet = dir.path().join("quiet.mp4");
    export_sequence(&quiet_project, SEQ, &paths, &quiet, &opts, |_, _| true).unwrap();

    let loud_peak = peak_amplitude(&loud, opts.sample_rate);
    let quiet_peak = peak_amplitude(&quiet, opts.sample_rate);
    // -12dB is ~0.25x amplitude; allow slack for lossy AAC.
    assert!(
        quiet_peak < loud_peak * 0.5,
        "-12dB should be clearly quieter (loud {loud_peak}, quiet {quiet_peak})"
    );
}

#[test]
fn a_delayed_audio_clip_lands_at_its_timeline_position() {
    // Sync is the property that matters most and the easiest to get subtly
    // wrong. An audio clip placed in the second half of the timeline must
    // leave the first half silent in the exported file.
    let duration = timeline::TIMEBASE * 2;
    let (mut project, paths) = setup_with_audio(duration);
    {
        let clip = &mut project.sequences[0].tracks[1].clips[0];
        clip.timeline_in = TimeTick(timeline::TIMEBASE);
        clip.timeline_out = TimeTick(duration);
        clip.source_out = TimeTick(timeline::TIMEBASE);
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("delayed.mp4");
    let opts = ExportOptions { quality: QualityPreset::Draft, ..Default::default() };

    export_sequence(&project, SEQ, &paths, &out, &opts, |_, _| true).unwrap();

    let rate = opts.sample_rate;
    let mut stream = media_ffmpeg::AudioDecoderStream::open(&out, rate, 2).unwrap();
    let mut all = Vec::new();
    while let Some(c) = stream.next_samples().unwrap() {
        all.extend_from_slice(&c);
    }

    // Compare windows held *clear* of the 1s cut rather than splitting the
    // decoded buffer in half. Two reasons the naive split is wrong here:
    // the decoder returns slightly more than 2s (AAC codes in whole 1024-
    // sample frames, so the tail is padded), which pushes the midpoint past
    // the cut; and a peak measurement trips on a single loud sample, so even
    // a 128-frame overlap into the second half fails the assertion while the
    // file is perfectly correct. Verified independently with ffmpeg's
    // volumedetect: -72 dB before the cut, -23 dB after.
    let frame = |secs: f64| (secs * rate as f64) as usize * 2;
    let peak_of = |s: &[f32]| s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let before = peak_of(&all[frame(0.05)..frame(0.95)]);
    let after = peak_of(&all[frame(1.05)..frame(1.95)]);
    assert!(
        before < after * 0.05,
        "audio before the clip's 1s start should be silent, not {before} (vs {after} after it)"
    );
}

#[test]
fn a_dissolve_exports_with_both_of_its_inputs_present() {
    // The specific failure this guards: export used to walk `track_plans` and
    // read only `active_clip`, so a transition's *outgoing* layer would have no
    // decoded frame. That renders a dissolve from a missing input — visible in
    // the file as a half-transition, and reported nowhere. Now both export and
    // preview drive off `CompiledFrameGraph::media_requests`, and
    // `frames_with_missing_sources` is the signal that would catch a regression.
    let (mut project, paths) = setup(timeline::TIMEBASE * 2);
    let asset_id = project.assets[0].id;
    let cut = timeline::TIMEBASE;

    // Split into two abutting clips with handles either side of the cut, then
    // put a dissolve on it.
    let track = &mut project.sequences[0].tracks[0];
    track.clips[0].timeline_out = TimeTick(cut);
    track.clips[0].source_out = TimeTick(timeline::TIMEBASE * 2); // tail handle
    track.clips.push(ClipInstance {
        id: ClipInstanceId(2),
        source: ClipSource::Media(asset_id),
        source_in: TimeTick(cut),
        source_out: TimeTick(timeline::TIMEBASE * 2),
        timeline_in: TimeTick(cut),
        timeline_out: TimeTick(timeline::TIMEBASE * 2),
        speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
        effects: vec![],
        audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
        audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
        linked_group: None,
    });
    track.transitions = vec![timeline::Transition {
        id: timeline::TransitionId(1),
        kind: timeline::TransitionKind::CrossDissolve,
        at: TimeTick(cut),
        duration: TimeTick(timeline::TIMEBASE / 2),
    }];

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dissolve.mp4");
    let stats = export::export_sequence(
        &project,
        SEQ,
        &paths,
        &out,
        &export::ExportOptions::default(),
        |_, _| true,
    )
    .expect("export should succeed");

    assert_eq!(
        stats.frames_with_missing_sources, 0,
        "every frame — including the {} inside the dissolve — must have all its inputs decoded",
        (timeline::TIMEBASE / 2) / timeline::FrameRate::Fps30.ticks_per_frame()
    );
    assert!(stats.frames_written > 0);
    assert!(out.exists());
}

#[test]
fn a_range_export_writes_only_that_range_and_starts_at_zero() {
    // Two independent things have to be right: the *content* comes from the
    // middle of the sequence, and the *timestamps* start at zero. Getting the
    // second wrong produces a file that opens with seconds of nothing, which
    // looks like a broken export rather than a range one.
    let (project, paths) = setup(timeline::TIMEBASE * 4);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("range.mp4");

    let stats = export::export_sequence(
        &project,
        SEQ,
        &paths,
        &out,
        &export::ExportOptions {
            range_ticks: Some((timeline::TIMEBASE, timeline::TIMEBASE * 3)),
            ..Default::default()
        },
        |_, _| true,
    )
    .expect("range export should succeed");

    let expected = 2 * 30; // 2 seconds at 30fps
    assert_eq!(
        stats.frames_written, expected,
        "a 1s..3s range of a 4s sequence should be {expected} frames"
    );

    // And the file itself agrees, rather than just the stats.
    media_ffmpeg::init().unwrap();
    let probed = media_ffmpeg::probe(&out).unwrap();
    let seconds = probed.duration_ticks as f64 / timeline::TIMEBASE as f64;
    assert!(
        (seconds - 2.0).abs() < 0.2,
        "the written file should be ~2s long, got {seconds:.3}s"
    );
}

#[test]
fn an_inverted_or_empty_range_is_refused_rather_than_writing_an_empty_file() {
    let (project, paths) = setup(timeline::TIMEBASE * 2);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("bad.mp4");

    for range in [
        (timeline::TIMEBASE, timeline::TIMEBASE),         // zero length
        (timeline::TIMEBASE * 2, timeline::TIMEBASE),     // inverted
        (timeline::TIMEBASE * 9, timeline::TIMEBASE * 10), // entirely past the end
    ] {
        let err = export::export_sequence(
            &project,
            SEQ,
            &paths,
            &out,
            &export::ExportOptions { range_ticks: Some(range), ..Default::default() },
            |_, _| true,
        );
        assert!(
            matches!(err, Err(export::ExportError::EmptyRange)),
            "range {range:?} should be refused, got {err:?}"
        );
    }
}

#[test]
fn quality_presets_trade_size_for_quality_in_the_expected_direction() {
    // The presets exist so a caller picks an intent instead of a CRF number.
    // If Draft and Master were wired backwards the export would still succeed
    // and nothing else would notice — file size is the observable that pins it.
    let (project, paths) = setup(timeline::TIMEBASE);
    let dir = tempfile::tempdir().unwrap();

    let mut sizes = Vec::new();
    for quality in [export::QualityPreset::Draft, export::QualityPreset::Master] {
        let out = dir.path().join(format!("{quality:?}.mp4"));
        export::export_sequence(
            &project,
            SEQ,
            &paths,
            &out,
            &export::ExportOptions { quality, ..Default::default() },
            |_, _| true,
        )
        .expect("export should succeed");
        sizes.push(std::fs::metadata(&out).unwrap().len());
    }

    assert!(
        sizes[1] > sizes[0],
        "Master should produce a larger file than Draft, got {} vs {}",
        sizes[1],
        sizes[0]
    );
}
