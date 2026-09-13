//! Tests for `EditorState`.
//!
//! Still one file: these were written against `EditorState` as a whole and
//! share a set of fixture builders, so splitting them to mirror the module
//! split is a separate job from splitting the production code.

use super::*;

/// The words `state` will actually hand out for `clip`, or `None` when it
/// has none to give — either never transcribed, or transcribed before an
/// edit that moved them. Most tests only care about that distinction; the
/// ones that care *which* of the two it is match on `transcript` directly.
fn words_of(state: &EditorState, clip: ClipInstanceId) -> Option<Vec<TimelineWord>> {
    match state.transcript(clip) {
        Transcript::Ready(words) => Some(words.to_vec()),
        Transcript::Missing | Transcript::Stale => None,
    }
}

fn fake_asset(id: u128, path: &str) -> media::MediaAsset {
    media::MediaAsset {
        id: media::MediaAssetId(id),
        original_absolute_path: path.into(),
        content_hash: [7u8; 32],
        container_format: "mp4".into(),
        video: None,
        audio: None,
        duration_ticks: TIMEBASE,
    }
}

/// Builds a state whose project has deliberately high IDs, so a loader
/// that resets `next_id` to 1 would immediately collide.
fn state_with_high_ids() -> EditorState {
    let mut state = EditorState::new();
    let mut project = (**state.project()).clone();
    let seq = &mut project.sequences[0];
    seq.tracks.push(Track {
        id: TrackId(500),
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![ClipInstance {
            id: ClipInstanceId(900),
            source: ClipSource::Media(media::MediaAssetId(1)),
            source_in: TimeTick(0),
            source_out: TimeTick(TIMEBASE),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(TIMEBASE),
            speed: SpeedCurve::Constant {
                numerator: 1,
                denominator: 1,
            },
            effects: vec![timeline::EffectInstance {
                id: timeline::EffectInstanceId(1234),
                effect_type: "gaussian_blur".into(),
                enabled: true,
                params: Default::default(),
            }],
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
    project.assets.push(fake_asset(1, "C:/media/clip.mp4"));
    state.undo.push("setup", std::sync::Arc::new(project));
    state
}

fn video_asset(id: u128, w: u32, h: u32, fps: (u32, u32)) -> media::MediaAsset {
    let mut a = fake_asset(id, "C:/media/clip.mp4");
    a.video = Some(media::VideoStreamInfo {
        width: w,
        height: h,
        pixel_format: media::PixelFormat::Yuv420p8,
        color: media::ColorMetadata {
            primaries: media::ColorPrimaries::Rec709,
            transfer: media::TransferFunction::Bt709,
            matrix: media::MatrixCoefficients::Bt709,
            full_range: false,
        },
        frame_rate: media::FrameRateKind::Constant(media::Rational {
            num: fps.0,
            den: fps.1,
        }),
        start_timecode: None,
        keyframe_index: vec![],
        pts_index: vec![],
    });
    a
}

#[test]
fn first_clip_resizes_the_sequence_to_match_it() {
    // Otherwise a 640x360 clip renders as a small island inside the
    // default 1920x1080 frame, in both the preview and the export.
    let mut state = EditorState::new();
    assert_eq!(
        (
            state.sequence().settings.width,
            state.sequence().settings.height
        ),
        (1920, 1080)
    );

    let asset = video_asset(1, 640, 360, (25, 1));
    let mut project = (**state.project()).clone();
    project.assets.push(asset.clone());
    state.undo.push("import", std::sync::Arc::new(project));
    state.append_asset_to_timeline(asset.id);

    let settings = &state.sequence().settings;
    assert_eq!((settings.width, settings.height), (640, 360));
    assert_eq!(settings.frame_rate, timeline::FrameRate::Fps25);
}

#[test]
fn a_second_clip_does_not_re_frame_an_edit_in_progress() {
    let mut state = EditorState::new();
    let first = video_asset(1, 640, 360, (30, 1));
    let second = video_asset(2, 1920, 1080, (60, 1));
    let mut project = (**state.project()).clone();
    project.assets.push(first.clone());
    project.assets.push(second.clone());
    state.undo.push("import", std::sync::Arc::new(project));

    state.append_asset_to_timeline(first.id);
    state.append_asset_to_timeline(second.id);

    let settings = &state.sequence().settings;
    assert_eq!(
        (settings.width, settings.height),
        (640, 360),
        "the sequence must keep the format it adopted from the first clip"
    );
    assert_eq!(settings.frame_rate, timeline::FrameRate::Fps30);
}

#[test]
fn non_standard_and_variable_source_rates_do_not_become_sequence_rates() {
    use media::{FrameRateKind, Rational};
    // A 47.113fps screen recording is a fact about the source, not a
    // sensible sequence rate — adopting it would make every timecode in
    // the sequence non-standard.
    assert_eq!(
        standard_frame_rate(&FrameRateKind::Constant(Rational {
            num: 47113,
            den: 1000
        })),
        None
    );
    // VFR's nominal rate is explicitly display-only.
    assert_eq!(
        standard_frame_rate(&FrameRateKind::Variable {
            nominal: Rational { num: 30, den: 1 }
        }),
        None
    );
    // Equivalent-ratio spellings of a standard rate must still match.
    assert_eq!(
        standard_frame_rate(&FrameRateKind::Constant(Rational {
            num: 60000,
            den: 2000
        })),
        Some(timeline::FrameRate::Fps30)
    );
    assert_eq!(
        standard_frame_rate(&FrameRateKind::Constant(Rational {
            num: 30000,
            den: 1001
        })),
        Some(timeline::FrameRate::Fps29_97)
    );
}

/// End-to-end over the path a user actually walks: import a real file,
/// add it to the timeline, export, and check the result fills the frame
/// at the source's native size.
///
/// Lives here rather than in `tests/` because `app` is a binary crate,
/// so integration tests can't reach its modules. It's the combination
/// that matters: before the sequence adopted the first clip's format,
/// both halves were individually correct and still produced a 640x360
/// picture stranded inside a 1920x1080 black frame.
/// Guards the invariant the drag-and-drop batching in `main.rs` relies
/// on: one call, one undo step, however many files came in. winit fires a
/// separate `DroppedFile` event per file, so the event loop accumulates
/// them and flushes once — if this ever became one undo entry per asset,
/// a 10-file drop would silently need 10 undos to back out.
#[test]
fn importing_several_files_at_once_is_a_single_undo_step() {
    media_ffmpeg::init().unwrap();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("test_fixtures");
    let mut state = EditorState::new();
    let before = state.undo.history().len();

    state.import_assets(vec![
        fixtures.join("test_playback_demo.mp4"),
        fixtures.join("test_h264.mp4"),
    ]);

    assert_eq!(state.project().assets.len(), 2, "both files should import");
    assert_eq!(
        state.undo.history().len() - before,
        1,
        "a multi-file import must be one undoable action, not one per file"
    );
}

#[test]
fn import_add_export_fills_the_frame_at_native_resolution() {
    media_ffmpeg::init().unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("test_fixtures")
        .join("test_playback_demo.mp4"); // 640x360
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.mp4");

    let mut state = EditorState::new();
    state.import_assets(vec![source.clone()]);
    let asset_id = state.project().assets[0].id;
    state.append_asset_to_timeline(asset_id);

    let settings = &state.sequence().settings;
    assert_eq!(
        (settings.width, settings.height),
        (640, 360),
        "sequence should have adopted the imported clip's format"
    );

    let stats = export::export_sequence(
        state.project(),
        state.seq_id,
        &state.asset_paths,
        &out,
        &export::ExportOptions {
            quality: export::QualityPreset::Draft,
            ..Default::default()
        },
        |_, _| true,
    )
    .expect("export should succeed");
    assert_eq!(stats.frames_with_missing_sources, 0);
    assert_eq!((stats.width, stats.height), (640, 360));

    // Fills the frame: compare the exported frame's left/right edge
    // columns against the source's. Letterboxing shows up as black
    // borders the source doesn't have — the exact regression this
    // guards, and one a dimensions-only assertion would miss.
    let mid = timeline::TIMEBASE / 2;
    let exported = media_ffmpeg::decode_frame_at(&out, mid).unwrap();
    let src = media_ffmpeg::decode_frame_at(&source, mid).unwrap();
    let edge_mean = |f: &media_ffmpeg::DecodedRgbaFrame, left: bool| -> f64 {
        let w = f.width as usize;
        let (mut total, mut count) = (0u64, 0u64);
        for y in 0..f.height as usize {
            let p = (y * w + if left { 0 } else { w - 1 }) * 4;
            total += f.rgba[p] as u64 + f.rgba[p + 1] as u64 + f.rgba[p + 2] as u64;
            count += 3;
        }
        total as f64 / count as f64
    };
    for left in [true, false] {
        let got = edge_mean(&exported, left);
        let want = edge_mean(&src, left);
        assert!(
            (got - want).abs() < 40.0,
            "exported {} edge (mean {got:.1}) should match the source's ({want:.1}) — a large \
             gap means the picture was letterboxed instead of filling the frame",
            if left { "left" } else { "right" }
        );
    }
}

// --- bins ------------------------------------------------------------

/// A state with two imported assets, ready to be filed into bins.
fn state_with_two_assets() -> EditorState {
    let mut state = EditorState::new();
    let mut project = (**state.project()).clone();
    project.assets.push(fake_asset(1, "C:/media/a.mp4"));
    project.assets.push(fake_asset(2, "C:/media/b.mp4"));
    state.undo.push("import", std::sync::Arc::new(project));
    state
}

#[test]
fn a_new_bin_starts_empty_and_everything_stays_at_the_root() {
    let mut state = state_with_two_assets();
    let bin = state.create_bin("Footage", None);

    assert_eq!(state.project().child_bins(None).len(), 1);
    assert_eq!(
        state.project().root_items().len(),
        3,
        "2 assets + 1 sequence still at root"
    );
    assert!(state
        .project()
        .bins
        .iter()
        .find(|b| b.id == bin)
        .unwrap()
        .items
        .is_empty());
}

#[test]
fn moving_an_item_into_a_bin_removes_it_from_the_root() {
    let mut state = state_with_two_assets();
    let bin = state.create_bin("Footage", None);
    let item = timeline::BinItem::Asset(media::MediaAssetId(1));

    state.move_item_to_bin(item, Some(bin));

    assert_eq!(state.project().bin_of(item), Some(bin));
    assert!(!state.project().root_items().contains(&item));
    // And back out again.
    state.move_item_to_bin(item, None);
    assert_eq!(state.project().bin_of(item), None);
    assert!(state.project().root_items().contains(&item));
}

#[test]
fn an_item_can_only_be_in_one_bin_at_a_time() {
    // Otherwise `bin_of` and `root_items` would disagree about where an
    // item lives, and the panel would draw it twice.
    let mut state = state_with_two_assets();
    let a = state.create_bin("A", None);
    let b = state.create_bin("B", None);
    let item = timeline::BinItem::Asset(media::MediaAssetId(1));

    state.move_item_to_bin(item, Some(a));
    state.move_item_to_bin(item, Some(b));

    let holding: Vec<u64> = state
        .project()
        .bins
        .iter()
        .filter(|bin| bin.items.contains(&item))
        .map(|bin| bin.id.0)
        .collect();
    assert_eq!(
        holding,
        vec![b.0],
        "only the destination bin should hold it"
    );
}

#[test]
fn deleting_a_bin_keeps_its_contents_and_promotes_child_bins() {
    // The data-loss guard: a folder delete must never take the footage
    // with it.
    let mut state = state_with_two_assets();
    let parent = state.create_bin("Parent", None);
    let child = state.create_bin("Child", Some(parent));
    let item = timeline::BinItem::Asset(media::MediaAssetId(1));
    state.move_item_to_bin(item, Some(child));

    state.delete_bin(child);

    assert!(
        state.project().bins.iter().all(|b| b.id != child),
        "bin is gone"
    );
    assert_eq!(
        state.project().bin_of(item),
        Some(parent),
        "its contents should move up to the parent, not vanish"
    );
    assert!(
        state
            .project()
            .assets
            .iter()
            .any(|a| a.id == media::MediaAssetId(1)),
        "the asset itself must still be in the project"
    );
}

#[test]
fn deleting_a_top_level_bin_returns_its_items_to_the_root() {
    let mut state = state_with_two_assets();
    let bin = state.create_bin("Temp", None);
    let item = timeline::BinItem::Asset(media::MediaAssetId(1));
    state.move_item_to_bin(item, Some(bin));

    state.delete_bin(bin);

    assert_eq!(state.project().bin_of(item), None);
    assert!(state.project().root_items().contains(&item));
}

#[test]
fn a_bin_cannot_be_moved_into_its_own_subtree() {
    // Would make the panel's recursive tree walk infinite.
    let mut state = state_with_two_assets();
    let parent = state.create_bin("Parent", None);
    let child = state.create_bin("Child", Some(parent));

    state.move_bin(parent, Some(child));

    assert_eq!(
        state
            .project()
            .bins
            .iter()
            .find(|b| b.id == parent)
            .unwrap()
            .parent,
        None,
        "the illegal move should have been refused"
    );
    assert!(
        !state.status.is_empty(),
        "and the refusal should be visible to the user"
    );
}

#[test]
fn bin_edits_are_undoable() {
    let mut state = state_with_two_assets();
    let bin = state.create_bin("Footage", None);
    let item = timeline::BinItem::Asset(media::MediaAssetId(1));
    state.move_item_to_bin(item, Some(bin));
    assert_eq!(state.project().bin_of(item), Some(bin));

    state.undo.undo(); // un-move
    assert_eq!(state.project().bin_of(item), None);
    state.undo.undo(); // un-create
    assert!(state.project().bins.is_empty());
    state.undo.redo();
    assert_eq!(state.project().bins.len(), 1);
}

#[test]
fn renaming_a_bin_to_the_same_name_adds_no_undo_step() {
    let mut state = state_with_two_assets();
    let bin = state.create_bin("Footage", None);
    state.rename_bin(bin, "Footage");

    // One undo returns to "no bins" — proving the no-op rename didn't
    // push a step of its own that would need undoing first.
    state.undo.undo();
    assert!(state.project().bins.is_empty());
}

#[test]
fn save_load_round_trips_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.nleproj");
    let mut state = state_with_high_ids();
    let before = (**state.project()).clone();

    state.save_to(&path);
    assert_eq!(state.project_path.as_deref(), Some(path.as_path()));

    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&path));
    assert_eq!(**fresh.project(), before);
}

#[test]
fn open_from_clears_stale_transcripts_from_the_previous_project() {
    // A freshly loaded project's clip ids restart from a low number
    // (see `loading_advances_next_id_past_every_existing_id`'s sibling
    // guarantee below), so they can easily collide with ids left over
    // from whatever project was open before. A leftover transcript
    // entry surviving that switch would silently attach itself to an
    // unrelated clip in the new project — wrong words shown, and
    // `delete_word_range` ripple-deleting footage based on stale ticks.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.nleproj");
    state_with_high_ids().save_to(&path);

    // Transcribed against a clip that really exists in the outgoing
    // project, so this is the id collision as it would actually happen.
    let (mut fresh, ids) = state_with_three_clips();
    fresh.store_words(ids[0], vec![TimelineWord { text: "stale".into(), start_tick: 0, end_tick: TIMEBASE }]);
    assert!(words_of(&fresh, ids[0]).is_some(), "fixture should start with a transcript to lose");

    assert!(fresh.open_from(&path));

    assert!(fresh.transcripts.is_empty(), "transcripts from the previous project must not survive open_from");
}

#[test]
fn open_from_clears_the_previous_projects_in_and_out_marks() {
    // Marks are positions in a sequence, so they mean nothing in a
    // different one. Left in place they draw markers over the newly
    // opened timeline at ticks nothing put there, and "export range"
    // silently scopes the export to them.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.nleproj");
    state_with_high_ids().save_to(&path);

    let mut fresh = EditorState::new();
    fresh.in_point = Some(TIMEBASE);
    fresh.out_point = Some(TIMEBASE * 5);
    assert!(fresh.open_from(&path));

    assert_eq!((fresh.in_point, fresh.out_point), (None, None));
    assert_eq!(fresh.marked_range(), None, "a range from the previous project must not survive open_from");
}

#[test]
fn loading_advances_next_id_past_every_existing_id() {
    // Regression guard: a loader that left `next_id` at 1 would hand out
    // IDs that already exist, so lookups by ID would match the wrong
    // clip and edits would corrupt unrelated parts of the timeline.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.nleproj");
    state_with_high_ids().save_to(&path);

    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&path));

    // 1234 is the highest ID in the fixture (an effect instance) —
    // proof this considers effect IDs, not just clips/tracks.
    assert_eq!(fresh.next_id(), 1235);
}

#[test]
fn opening_a_bad_file_leaves_the_current_project_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("garbage.nleproj");
    std::fs::write(&path, b"not a cbor document").unwrap();

    let mut state = state_with_high_ids();
    let before = (**state.project()).clone();
    assert!(!state.open_from(&path));
    assert_eq!(
        **state.project(),
        before,
        "a failed open must not destroy open work"
    );
    assert!(state.project_path.is_none());
}

#[test]
fn media_saved_beside_the_project_gets_a_relative_reference() {
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("footage").join("a.mp4");
    std::fs::create_dir_all(media.parent().unwrap()).unwrap();
    std::fs::write(&media, b"x").unwrap();

    let mut state = EditorState::new();
    let mut project = (**state.project()).clone();
    project.assets.push(fake_asset(1, &media.to_string_lossy()));
    state.undo.push("setup", std::sync::Arc::new(project));

    let path = dir.path().join("p.nleproj");
    state.save_to(&path);
    let doc = project::load(&path).unwrap();

    let rel = &doc.media_references[0].relative_path;
    assert!(
        !std::path::Path::new(rel).is_absolute(),
        "media under the project dir should be stored relative, got {rel:?}"
    );
    // And it must resolve back to the real file from the project's dir.
    assert!(dir.path().join(rel).exists());
}

// ---- Scene-cut detection: the pure tick-mapping half ---------------
//
// `scene_cut_ops` is tested directly with hand-supplied "detected" source
// ticks rather than through `detect_scene_cuts`'s real decode+histogram
// pipeline — that pipeline is exercised by a real-footage smoke test
// instead (env-gated, matching the rest of this project's real-footage
// coverage), since decoding is slow and this half is the actual risk in
// the wiring: source-tick -> timeline-tick mapping, speed inversion,
// boundary filtering. The detection *algorithm* itself is already fully
// covered, with hand-derived expected values, in
// `crates/render/tests/scene_cut.rs`.

#[test]
fn scene_cut_ops_maps_a_source_tick_straight_through_at_1x_with_no_trim() {
    let (mut state, _) = state_with_three_clips();
    let clip = state.sequence().tracks[0].clips[0].clone();
    let track = state.sequence().tracks[0].id;
    assert_eq!(clip.timeline_in, TimeTick(0));
    assert_eq!(clip.source_in, TimeTick(0));

    let ops = state.scene_cut_ops(track, &clip, &[TIMEBASE / 2]);
    assert_eq!(ops.len(), 1);
    let EditOp::Razor { track: op_track, at, .. } = ops[0] else { panic!("expected a Razor op") };
    assert_eq!(op_track, track);
    assert_eq!(at, TimeTick(TIMEBASE / 2));
}

#[test]
fn scene_cut_ops_accounts_for_both_timeline_placement_and_source_trim() {
    let (mut state, _) = state_with_three_clips();
    // The middle clip: starts at 1s on the timeline, but (per the
    // fixture) also starts at 0 in its own source — trim it in so the
    // two offsets are genuinely different and a bug that mixed them up
    // would be caught.
    let track = state.sequence().tracks[0].id;
    let mut clip = state.sequence().tracks[0].clips[1].clone();
    clip.source_in = TimeTick(TIMEBASE / 4); // trimmed a quarter-second into its source
    // timeline_in stays at 1s (from the fixture).

    // A cut detected 0.3s into the (already-trimmed) source should land
    // at timeline 1s + 0.3s, independent of the 0.25s trim offset itself
    // — trim moves *where in the source* ticks 0 corresponds to, not how
    // far a detected cut is from that point.
    let source_cut = clip.source_in.0 + TIMEBASE * 3 / 10;
    let ops = state.scene_cut_ops(track, &clip, &[source_cut]);
    assert_eq!(ops.len(), 1);
    let EditOp::Razor { at, .. } = ops[0] else { panic!("expected a Razor op") };
    assert_eq!(at, TimeTick(clip.timeline_in.0 + TIMEBASE * 3 / 10));
}

#[test]
fn scene_cut_ops_inverts_speed_for_a_retimed_clip() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let mut clip = state.sequence().tracks[0].clips[0].clone();
    // 2x speed: twice as much source passes per unit of timeline time, so
    // a cut 1s into the source lands at 0.5s on the timeline. Widen
    // source_out to match — a real 2x clip covering 1s of timeline
    // consumes 2s of source, and leaving it at the fixture's 1x value
    // would put this test's own cut right on (or past) the boundary
    // `scene_cut_ops` correctly rejects.
    clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
    clip.source_out = TimeTick(clip.source_in.0 + TIMEBASE * 2);

    let ops = state.scene_cut_ops(track, &clip, &[TIMEBASE]);
    assert_eq!(ops.len(), 1);
    let EditOp::Razor { at, .. } = ops[0] else { panic!("expected a Razor op") };
    assert_eq!(at, TimeTick(TIMEBASE / 2), "2x speed should halve the timeline offset");
}

#[test]
fn scene_cut_ops_refuses_a_keyframed_speed_clip() {
    // Same limitation as clip-speed audio retiming, and for the same
    // reason: inverting a keyframed curve to recover a timeline tick from
    // a source tick isn't a cheap closed-form operation the way constant
    // speed is.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let mut clip = state.sequence().tracks[0].clips[0].clone();
    clip.speed = SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0)));

    let ops = state.scene_cut_ops(track, &clip, &[TIMEBASE / 2]);
    assert!(ops.is_empty(), "a keyframed-speed clip should produce no ops, not a wrong mapping");
}

#[test]
fn scene_cut_ops_drops_a_cut_at_or_beyond_the_clips_own_source_bounds() {
    // A "cut" detected exactly at source_in or at/after source_out isn't
    // a split inside the clip's own material — applying it would ask
    // Razor to cut at the clip's own edge (or beyond it), which is not
    // what "detect the cuts inside this clip" means.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let clip = state.sequence().tracks[0].clips[0].clone();
    assert_eq!(clip.source_in, TimeTick(0));
    assert_eq!(clip.source_out, TimeTick(TIMEBASE));

    let ops = state.scene_cut_ops(track, &clip, &[0, TIMEBASE, TIMEBASE * 2]);
    assert!(ops.is_empty(), "cuts at/outside the clip's own bounds must be dropped, got {ops:?}");
}

#[test]
fn scene_cut_ops_produces_one_op_with_a_distinct_id_per_cut() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let clip = state.sequence().tracks[0].clips[2].clone(); // the 2s-3s clip
    let base = clip.source_in.0;

    let ops = state.scene_cut_ops(track, &clip, &[base + TIMEBASE / 4, base + TIMEBASE / 2]);
    assert_eq!(ops.len(), 2);
    let ids: Vec<ClipInstanceId> = ops
        .iter()
        .map(|op| match op {
            EditOp::Razor { new_clip_id, .. } => *new_clip_id,
            _ => panic!("expected a Razor op"),
        })
        .collect();
    assert_ne!(ids[0], ids[1], "each cut needs its own new clip id");
}

// ---- Silence-based auto-cut: the pure ripple-delete-planning half --
//
// `silence_removal_ops` is tested with hand-supplied "detected" gaps,
// same reasoning as `scene_cut_ops` above: real decoding is covered by
// a real-footage smoke test, and this half is where the actual wiring
// risk lives — tick mapping, right-to-left ordering so ripple deletes
// don't invalidate not-yet-processed ranges, and which of the two
// Razor-created pieces is the one to Extract.

#[test]
fn silence_removal_ops_produces_two_razors_and_one_extract_per_gap() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let clip = state.sequence().tracks[0].clips[0].clone(); // 0-1s clip, source_in=0
    let ops = state.silence_removal_ops(track, &clip, &[(0.2, 0.4)]);
    assert_eq!(ops.len(), 3, "one gap should be two Razor ops plus one Extract, got {ops:?}");
    let razor_ticks: Vec<i64> = ops
        .iter()
        .filter_map(|op| match op {
            EditOp::Razor { at, .. } => Some(at.0),
            _ => None,
        })
        .collect();
    assert_eq!(razor_ticks, vec![TIMEBASE / 5, TIMEBASE * 2 / 5], "expected razors at 0.2s and 0.4s");
    assert!(matches!(ops[2], EditOp::Extract { .. }), "the gap piece should be extracted last");
}

#[test]
fn silence_removal_ops_extracts_the_clip_between_the_two_razors() {
    // The subtle part: `Razor`'s `new_clip_id` becomes the *right-hand*
    // piece each time, so isolating [start, end) as its own clip means
    // extracting the id assigned to the *first* razor (at `start`) — by
    // the second razor (at `end`), that same clip is the *left*-hand
    // piece of *that* split, which is why it keeps that id rather than
    // getting a third one.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let clip = state.sequence().tracks[0].clips[0].clone();
    let ops = state.silence_removal_ops(track, &clip, &[(0.2, 0.4)]);
    let EditOp::Razor { new_clip_id: first_razor_id, .. } = ops[0] else { panic!() };
    let EditOp::Extract { clip: extracted } = ops[2] else { panic!() };
    assert_eq!(extracted, first_razor_id);
}

#[test]
fn silence_removal_ops_processes_multiple_gaps_rightmost_first() {
    // Rightmost first: extracting a gap ripples everything to its right
    // leftward. Processing right-to-left means every range still queued
    // is entirely to the *left* of whatever was just ripple-deleted, so
    // its precomputed tick positions are never invalidated by an earlier
    // step in this same batch.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let clip = state.sequence().tracks[0].clips[0].clone();
    let ops = state.silence_removal_ops(track, &clip, &[(0.1, 0.2), (0.6, 0.7)]);
    assert_eq!(ops.len(), 6, "two gaps should be six ops, got {ops:?}");
    let EditOp::Razor { at: first_at, .. } = ops[0] else { panic!() };
    assert_eq!(first_at, TimeTick(TIMEBASE * 6 / 10), "the rightmost gap (0.6s) must be processed first");
}

#[test]
fn silence_removal_ops_drops_a_gap_that_reaches_either_edge_of_the_clip() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let clip = state.sequence().tracks[0].clips[0].clone(); // 0-1s
    let ops = state.silence_removal_ops(track, &clip, &[(0.0, 0.1), (0.9, 1.0)]);
    assert!(ops.is_empty(), "gaps touching either edge aren't a razor-and-extract cut, got {ops:?}");
}

#[test]
fn silence_removal_ops_refuses_a_keyframed_speed_clip() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let mut clip = state.sequence().tracks[0].clips[0].clone();
    clip.speed = SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0)));
    let ops = state.silence_removal_ops(track, &clip, &[(0.2, 0.4)]);
    assert!(ops.is_empty());
}

/// Runs `detect_silence_and_ripple_delete` against a real file, decoding
/// real audio through the real `audio_source` path — same convention and
/// same reasoning as the scene-cut real-footage test just below: env-gated,
/// `#[ignore]`d, loose assertion (real footage's true silence count isn't
/// known ahead of time), proving the decode+detect+ripple-delete pipeline
/// runs end to end rather than pinning an exact number.
#[test]
#[ignore]
fn detect_silence_runs_end_to_end_on_real_footage() {
    let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => panic!(
            "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
        ),
    };
    media_ffmpeg::init().unwrap();
    let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
    assert!(asset.audio.is_some(), "this test needs footage with an audio track");
    let video = asset.video.as_ref().expect("expected a video stream");
    let duration = asset.duration_ticks.min(TIMEBASE * 20);

    let mut state = EditorState::new();
    state.asset_paths.insert(asset.id, path);
    let mut project = (**state.project()).clone();
    let track_id = TrackId(1);
    project.sequences[0].settings.width = video.width;
    project.sequences[0].settings.height = video.height;
    project.assets.push(asset.clone());
    project.sequences[0].tracks.push(Track {
        id: track_id,
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![ClipInstance {
            id: ClipInstanceId(1),
            source: ClipSource::Media(asset.id),
            source_in: TimeTick(0),
            source_out: TimeTick(duration),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(duration),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }],
        transitions: vec![],
        gain_db: timeline::unity_gain(),
        pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
    state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
    // The fixture above hand-picks ClipInstanceId(1) directly rather than
    // going through `next_id()` (which a real import/add path always
    // does) — so `next_id` must be advanced past it here, or the method
    // under test would allocate a colliding id the moment it calls
    // `self.next_id()` itself, corrupting the very project it's editing.
    state.next_id = 1000;

    let total_duration_before: i64 = state.sequence().tracks[0].clips.iter().map(|c| c.timeline_out.0 - c.timeline_in.0).sum();
    let removed = state.detect_silence_and_ripple_delete(ClipInstanceId(1));
    println!("real footage silence detection: {removed} gap(s) removed over {:.1}s of source, status: {:?}", duration as f64 / TIMEBASE as f64, state.status);

    let clips = &state.sequence().tracks[0].clips;
    assert_eq!(clips.len(), removed + 1, "expected one more clip than gaps removed");
    let mut sorted: Vec<_> = clips.iter().map(|c| (c.timeline_in.0, c.timeline_out.0)).collect();
    sorted.sort();
    for w in sorted.windows(2) {
        assert_eq!(w[0].1, w[1].0, "remaining pieces must abut with no gap or overlap: {sorted:?}");
    }
    let mut ids: Vec<_> = clips.iter().map(|c| c.id).collect();
    ids.sort_by_key(|id| id.0);
    ids.dedup();
    assert_eq!(ids.len(), clips.len(), "every resulting piece must have a distinct id");
    let total_duration_after: i64 = clips.iter().map(|c| c.timeline_out.0 - c.timeline_in.0).sum();
    assert!(
        total_duration_after <= total_duration_before,
        "ripple-deleting silence must never make the sequence longer"
    );
}

// ---- Beat-sync: the pure onset-to-marker mapping half --------------
//
// Same split as scene-cut and silence detection: `beat_markers` is
// tested with hand-supplied onset times, real decode+FFT is exercised by
// a real-footage smoke test. Markers, not razor cuts — a beat is
// something to *snap to* (the timeline's existing `snap_tick` already
// treats every marker as a snap candidate, so adding beat markers makes
// them magnetic for free, no new snapping code needed), not something
// that should silently restructure the timeline the way a scene cut or
// a silence removal does.

#[test]
fn beat_markers_maps_source_seconds_to_timeline_ticks() {
    let (mut state, _) = state_with_three_clips();
    let clip = state.sequence().tracks[0].clips[0].clone(); // 0-1s, source_in=0, 1x
    let markers = state.beat_markers(&clip, &[0.25, 0.5, 0.75]);
    assert_eq!(markers.len(), 3);
    let positions: Vec<i64> = markers.iter().map(|m| m.position.0).collect();
    assert_eq!(positions, vec![TIMEBASE / 4, TIMEBASE / 2, TIMEBASE * 3 / 4]);
}

#[test]
fn beat_markers_accounts_for_timeline_placement_and_source_trim() {
    let (mut state, _) = state_with_three_clips();
    let mut clip = state.sequence().tracks[0].clips[1].clone(); // starts at 1s on the timeline
    clip.source_in = TimeTick(TIMEBASE / 4);
    let markers = state.beat_markers(&clip, &[clip.source_in.0 as f64 / TIMEBASE as f64 + 0.3]);
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].position, TimeTick(clip.timeline_in.0 + TIMEBASE * 3 / 10));
}

#[test]
fn beat_markers_inverts_speed_for_a_retimed_clip() {
    let (mut state, _) = state_with_three_clips();
    let mut clip = state.sequence().tracks[0].clips[0].clone();
    clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
    clip.source_out = TimeTick(clip.source_in.0 + TIMEBASE * 2);
    let markers = state.beat_markers(&clip, &[1.0]);
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].position, TimeTick(TIMEBASE / 2), "2x speed should halve the timeline offset");
}

#[test]
fn beat_markers_drops_onsets_outside_the_clips_own_bounds() {
    let (mut state, _) = state_with_three_clips();
    let clip = state.sequence().tracks[0].clips[0].clone(); // 0-1s
    let markers = state.beat_markers(&clip, &[-0.1, 0.0, 0.5, 1.0, 1.5]);
    assert_eq!(markers.len(), 1, "only the 0.5s onset falls strictly inside the clip");
}

#[test]
fn beat_markers_refuses_a_keyframed_speed_clip() {
    let (mut state, _) = state_with_three_clips();
    let mut clip = state.sequence().tracks[0].clips[0].clone();
    clip.speed = SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0)));
    assert!(state.beat_markers(&clip, &[0.5]).is_empty());
}

#[test]
fn beat_markers_each_get_a_distinct_id() {
    let (mut state, _) = state_with_three_clips();
    let clip = state.sequence().tracks[0].clips[2].clone();
    let base = clip.source_in.0 as f64 / TIMEBASE as f64;
    let markers = state.beat_markers(&clip, &[base + 0.2, base + 0.5, base + 0.8]);
    assert_eq!(markers.len(), 3);
    let mut ids: Vec<_> = markers.iter().map(|m| m.id).collect();
    ids.sort_by_key(|id| id.0);
    ids.dedup();
    assert_eq!(ids.len(), 3, "each beat marker needs its own id");
}

/// Runs `detect_beats_and_add_markers` against a real file, exercising
/// the real decode+FFT path. Same conventions as the other real-footage
/// tests in this module: env-gated, `#[ignore]`d, loose assertion (this
/// screen recording has no music, so its true "beat" count — probably
/// driven by keyboard/mouse clicks rather than a musical pulse — isn't
/// known ahead of time). What this proves is that decoding, FFT-based
/// flux analysis, and adding real markers to a real project works end to
/// end on a real file, not that the detected events are musically
/// meaningful for this particular recording.
#[test]
#[ignore]
fn detect_beats_runs_end_to_end_on_real_footage() {
    let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => panic!(
            "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
        ),
    };
    media_ffmpeg::init().unwrap();
    let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
    assert!(asset.audio.is_some(), "this test needs footage with an audio track");
    let video = asset.video.as_ref().expect("expected a video stream");
    let duration = asset.duration_ticks.min(TIMEBASE * 20);

    let mut state = EditorState::new();
    state.asset_paths.insert(asset.id, path);
    let mut project = (**state.project()).clone();
    let track_id = TrackId(1);
    project.sequences[0].settings.width = video.width;
    project.sequences[0].settings.height = video.height;
    project.assets.push(asset.clone());
    project.sequences[0].tracks.push(Track {
        id: track_id,
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![ClipInstance {
            id: ClipInstanceId(1),
            source: ClipSource::Media(asset.id),
            source_in: TimeTick(0),
            source_out: TimeTick(duration),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(duration),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }],
        transitions: vec![],
        gain_db: timeline::unity_gain(),
        pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
    state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
    state.next_id = 1000;

    let added = state.detect_beats_and_add_markers(ClipInstanceId(1));
    println!("real footage beat detection: {added} marker(s) added over {:.1}s of source", duration as f64 / TIMEBASE as f64);

    let markers = &state.sequence().markers;
    assert_eq!(markers.len(), added, "the sequence should have exactly the reported number of markers");
    for m in markers {
        assert!(m.position.0 > 0 && m.position.0 < duration, "every marker must fall inside the analysed span, got {:?}", m.position);
    }
    let mut ids: Vec<_> = markers.iter().map(|m| m.id).collect();
    ids.sort_by_key(|id| id.0);
    ids.dedup();
    assert_eq!(ids.len(), markers.len(), "every marker must have a distinct id");
}

// ---- Loudness auto-match: the pure gain-application half -----------
//
// `apply_loudness_match_gain` takes an already-*computed* gain (from
// `audio::loudness::gain_to_reach_target`, itself tested with hand
// values in `crates/audio/tests/loudness.rs`) and writes it to the
// clip. Real decode + measurement is exercised by a real-footage smoke
// test — this half is where the actual wiring risk is: refusing to
// clobber existing fader automation, writing to the right clip.

#[test]
fn apply_loudness_match_gain_sets_the_clips_audio_gain() {
    let (mut state, ids) = state_with_three_clips();
    assert!(state.apply_loudness_match_gain(ids[0], 6.5));
    let clip = state.find_clip(ids[0]).unwrap().1;
    assert_eq!(clip.audio_gain_db.default, ParamValue::Number(6.5));
}

#[test]
fn apply_loudness_match_gain_is_undoable() {
    let (mut state, ids) = state_with_three_clips();
    state.apply_loudness_match_gain(ids[0], 6.5);
    state.undo.undo();
    let clip = state.find_clip(ids[0]).unwrap().1;
    assert_eq!(clip.audio_gain_db.default, ParamValue::Number(0.0), "undo should restore the original gain");
}

#[test]
fn apply_loudness_match_gain_refuses_a_clip_with_animated_gain() {
    // Loudness matching sets one constant value for the whole clip —
    // applying it to a clip whose gain is already keyframed would
    // silently discard that automation, which is real user work.
    let (mut state, ids) = state_with_three_clips();
    {
        let mut project = (**state.project()).clone();
        let clip = project.sequences[0].tracks[0].clips.iter_mut().find(|c| c.id == ids[0]).unwrap();
        clip.audio_gain_db.upsert_keyframe(TimeTick(0), ParamValue::Number(0.0), timeline::InterpolationMode::Linear);
        clip.audio_gain_db.upsert_keyframe(TimeTick(TIMEBASE), ParamValue::Number(3.0), timeline::InterpolationMode::Linear);
        state.undo.push("animate gain (test setup)", std::sync::Arc::new(project));
    }
    assert!(!state.apply_loudness_match_gain(ids[0], 6.5), "must refuse rather than overwrite automation");
}

#[test]
fn apply_loudness_match_gain_on_a_missing_clip_does_nothing() {
    let (mut state, _) = state_with_three_clips();
    assert!(!state.apply_loudness_match_gain(ClipInstanceId(99999), 6.5));
}

/// Runs `match_clip_loudness` against a real file — real decode, real
/// EBU R128 measurement, real gain applied. Unlike the other real-
/// footage tests here, this one's expected outcome *is* fully known
/// ahead of time (loudness matching is deterministic arithmetic on a
/// measured value), so the assertion checks the actual number rather
/// than just "ran without crashing".
#[test]
#[ignore]
fn match_clip_loudness_runs_end_to_end_on_real_footage() {
    let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => panic!(
            "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
        ),
    };
    media_ffmpeg::init().unwrap();
    let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
    assert!(asset.audio.is_some(), "this test needs footage with an audio track");
    let video = asset.video.as_ref().expect("expected a video stream");
    let duration = asset.duration_ticks.min(TIMEBASE * 20);

    let mut state = EditorState::new();
    state.asset_paths.insert(asset.id, path);
    let mut project = (**state.project()).clone();
    project.sequences[0].settings.width = video.width;
    project.sequences[0].settings.height = video.height;
    project.assets.push(asset.clone());
    project.sequences[0].tracks.push(Track {
        id: TrackId(1),
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![ClipInstance {
            id: ClipInstanceId(1),
            source: ClipSource::Media(asset.id),
            source_in: TimeTick(0),
            source_out: TimeTick(duration),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(duration),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }],
        transitions: vec![],
        gain_db: timeline::unity_gain(),
        pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
    state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
    state.next_id = 1000;

    let target = -14.0;
    let gain = state.match_clip_loudness(ClipInstanceId(1), target).expect("real footage should measure");
    println!("real footage loudness match: applied {gain:.2}dB toward {target} LUFS");

    let clip = state.find_clip(ClipInstanceId(1)).unwrap().1;
    assert_eq!(clip.audio_gain_db.default, ParamValue::Number(gain), "the clip must carry the reported gain");
    assert!((-24.0..=24.0).contains(&gain), "gain must stay within the documented clamp, got {gain}");

    // Re-measuring after applying the gain (by adding it to the raw
    // signal in dB, which is what a linear gain stage does) should land
    // at the target — the honest way to confirm this isn't just
    // "produced *a* number" but produced the *right* number.
    let sample_rate = state.sequence().settings.sample_rate;
    let interleaved = super::analysis::decode_interleaved(&state.asset_paths, sample_rate, &clip).unwrap();
    let raw = audio::loudness::LoudnessMeasurement::analyze(&interleaved, audio::CHANNELS, sample_rate);
    let achieved = raw.integrated_lufs as f64 + gain;
    if gain.abs() < 23.9 {
        // Only meaningful when the clamp didn't bite — a clamped gain by
        // definition won't reach the target.
        assert!((achieved - target).abs() < 0.5, "applying the computed gain should land near {target} LUFS, got {achieved:.2}");
    }
}

// ---- Warp Stabilizer: the pure keyframe-writing half ----------------
//
// `apply_stabilization_keyframes` takes already-computed `(timeline_tick,
// (offset_x, offset_y))` pairs and writes them as `Transform::POSITION`
// keyframes — real motion estimation and path smoothing are exercised by
// a real-footage smoke test and are already covered with synthetic
// ground truth in `crates/render/tests/stabilize.rs`. This half is where
// the wiring risk is: finding vs. creating the Transform effect, adding
// to (not replacing) any existing constant framing offset, and refusing
// to clobber existing position keyframes.

#[test]
fn apply_stabilization_keyframes_creates_a_transform_effect_when_none_exists() {
    let (mut state, ids) = state_with_three_clips();
    assert!(state.find_clip(ids[0]).unwrap().1.effects.is_empty());
    let applied = state.apply_stabilization_keyframes(ids[0], &[(0, (5.0, -3.0)), (TIMEBASE / 2, (2.0, 1.0))]);
    assert_eq!(applied, 2);
    let clip = state.find_clip(ids[0]).unwrap().1;
    let transform = clip.effects.iter().find(|e| e.effect_type == render::transform::TYPE_ID).unwrap();
    let pos = transform.params.get(render::transform::POSITION).unwrap();
    assert_eq!(pos.evaluate_at(TimeTick(0)), ParamValue::Vec2(5.0, -3.0));
    assert_eq!(pos.evaluate_at(TimeTick(TIMEBASE / 2)), ParamValue::Vec2(2.0, 1.0));
}

#[test]
fn apply_stabilization_keyframes_adds_to_an_existing_constant_position() {
    // A user-set constant framing offset (e.g. "shift the shot left a
    // bit") must survive stabilization layered on top of it, not get
    // silently discarded.
    let (mut state, ids) = state_with_three_clips();
    add_transform_effect(&mut state, ids[0], (100.0, 50.0));
    state.apply_stabilization_keyframes(ids[0], &[(0, (5.0, -3.0))]);
    let clip = state.find_clip(ids[0]).unwrap().1;
    let pos = clip.effects[0].params.get(render::transform::POSITION).unwrap();
    assert_eq!(pos.evaluate_at(TimeTick(0)), ParamValue::Vec2(105.0, 47.0));
}

#[test]
fn apply_stabilization_keyframes_refuses_an_already_animated_position() {
    let (mut state, ids) = state_with_three_clips();
    add_transform_effect(&mut state, ids[0], (0.0, 0.0));
    // `set_param_value_at` only writes a keyframe when the track is
    // *already* animated (otherwise it just overwrites `default`) — so
    // building the animated case needs the same direct project-mutation
    // approach `state_with_three_clips`'s siblings use elsewhere, not a
    // couple of ordinary param-value calls.
    {
        let mut project = (**state.project()).clone();
        let clip = project.sequences[0].tracks[0].clips.iter_mut().find(|c| c.id == ids[0]).unwrap();
        let pos = clip.effects[0].params.get_mut(render::transform::POSITION).unwrap();
        pos.upsert_keyframe(TimeTick(0), ParamValue::Vec2(1.0, 0.0), timeline::InterpolationMode::Linear);
        pos.upsert_keyframe(TimeTick(TIMEBASE / 2), ParamValue::Vec2(2.0, 0.0), timeline::InterpolationMode::Linear);
        state.undo.push("animate position (test setup)", std::sync::Arc::new(project));
    }
    let applied = state.apply_stabilization_keyframes(ids[0], &[(0, (5.0, -3.0))]);
    assert_eq!(applied, 0, "must refuse rather than overwrite existing position keyframes");
}

#[test]
fn apply_stabilization_keyframes_is_one_undo_step_regardless_of_sample_count() {
    let (mut state, ids) = state_with_three_clips();
    let depth = state.undo.history().len();
    state.apply_stabilization_keyframes(ids[0], &[(0, (1.0, 0.0)), (TIMEBASE / 3, (2.0, 0.0)), (TIMEBASE * 2 / 3, (3.0, 0.0))]);
    assert_eq!(state.undo.history().len(), depth + 1);
}

#[test]
fn apply_stabilization_keyframes_on_an_empty_input_does_nothing() {
    let (mut state, ids) = state_with_three_clips();
    let depth = state.undo.history().len();
    assert_eq!(state.apply_stabilization_keyframes(ids[0], &[]), 0);
    assert_eq!(state.undo.history().len(), depth);
}

fn add_transform_effect(state: &mut EditorState, clip_id: ClipInstanceId, position: (f64, f64)) {
    let mut params = std::collections::BTreeMap::new();
    params.insert(
        render::transform::POSITION.to_string(),
        timeline::ParamTrack::constant(ParamValue::Vec2(position.0, position.1)),
    );
    for p in render::transform::descriptor().params {
        params.entry(p.name.to_string()).or_insert_with(|| timeline::ParamTrack::constant(p.default));
    }
    let id = timeline::EffectInstanceId(state.next_id());
    state.add_effect(
        clip_id,
        timeline::EffectInstance {
            id,
            effect_type: render::transform::TYPE_ID.to_string(),
            enabled: true,
            params,
        },
    );
}

/// Runs `stabilize_clip` against a real file — real frame decode, real
/// downsampled block-matching motion estimation, real keyframes written.
/// Env-gated, `#[ignore]`d, same conventions as the rest of this module's
/// real-footage tests. The assertion checks that *something* measurable
/// happened (keyframes landed at the expected ticks, with the expected
/// count) rather than that the correction is visually good — motion
/// estimation quality on an arbitrary real clip isn't something a single
/// automated assertion can honestly judge; this is the same trade the
/// scene-cut and beat-detection real-footage tests make.
#[test]
#[ignore]
fn stabilize_clip_runs_end_to_end_on_real_footage() {
    let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => panic!(
            "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
        ),
    };
    media_ffmpeg::init().unwrap();
    let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
    let video = asset.video.as_ref().expect("expected a video stream");
    let duration = asset.duration_ticks.min(TIMEBASE * 10); // stabilization's per-sample decode+search is the slowest of these actions; keep the smoke test itself short

    let mut state = EditorState::new();
    state.asset_paths.insert(asset.id, path);
    let mut project = (**state.project()).clone();
    project.sequences[0].settings.width = video.width;
    project.sequences[0].settings.height = video.height;
    project.assets.push(asset.clone());
    project.sequences[0].tracks.push(Track {
        id: TrackId(1),
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![ClipInstance {
            id: ClipInstanceId(1),
            source: ClipSource::Media(asset.id),
            source_in: TimeTick(0),
            source_out: TimeTick(duration),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(duration),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }],
        transitions: vec![],
        gain_db: timeline::unity_gain(),
        pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
    state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
    state.next_id = 1000;

    let applied = state.stabilize_clip(ClipInstanceId(1));
    println!("real footage stabilization: {applied} keyframe(s) written over {:.1}s of source", duration as f64 / TIMEBASE as f64);
    assert!(applied >= 3, "at least the sampled frames should produce keyframes, got {applied}");

    let clip = state.find_clip(ClipInstanceId(1)).unwrap().1;
    let transform = clip.effects.iter().find(|e| e.effect_type == render::transform::TYPE_ID).expect("Transform effect should exist");
    let pos = transform.params.get(render::transform::POSITION).unwrap();
    assert!(pos.is_animated(), "position should be keyframed after stabilization");
    assert_eq!(pos.keyframes.len(), applied);
    for kf in &pos.keyframes {
        assert!(kf.at.0 >= 0 && kf.at.0 <= duration, "every keyframe must land inside the clip's own span, got {:?}", kf.at);
    }
}

// ---- Auto-captions: the pure clip-building half ---------------------
//
// `caption_ops` turns already-transcribed segments (hand-supplied here,
// real transcription exercised by a real-footage smoke test) into
// `Overwrite` ops placing a title clip per segment — same
// detect/apply split as every other analysis action in this file.

fn dummy_segment(text: &str, start_ms: u32, end_ms: u32) -> speech::Segment {
    speech::Segment { text: text.into(), start_ms, end_ms, words: vec![] }
}

#[test]
fn caption_ops_places_one_title_clip_per_segment_at_the_right_ticks() {
    let (mut state, ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let clip = state.find_clip(ids[0]).unwrap().1; // 0-1s clip
    let segments = vec![dummy_segment("Hello there", 0, 300), dummy_segment("General Kenobi", 300, 900)];
    let ops = state.caption_ops(track, &clip, &segments);
    assert_eq!(ops.len(), 2);
    let EditOp::Overwrite { at: at0, clip: clip0, .. } = &ops[0] else { panic!() };
    assert_eq!(*at0, TimeTick(0));
    assert_eq!(clip0.timeline_out.0, TIMEBASE * 3 / 10);
    let ClipSource::Title(spec0) = &clip0.source else { panic!("expected a title clip") };
    assert_eq!(spec0.text, "Hello there");

    let EditOp::Overwrite { at: at1, clip: clip1, .. } = &ops[1] else { panic!() };
    assert_eq!(*at1, TimeTick(TIMEBASE * 3 / 10));
    assert_eq!(clip1.timeline_out.0, TIMEBASE * 9 / 10);
}

#[test]
fn caption_ops_accounts_for_timeline_placement_and_source_trim() {
    let (mut state, ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let mut clip = state.find_clip(ids[1]).unwrap().1; // starts at 1s on the timeline
    clip.source_in = TimeTick(TIMEBASE / 4);
    let start_ms = 300; // 0.3s into the (trimmed) source
    let end_ms = 600;
    let ops = state.caption_ops(track, &clip, &[dummy_segment("hi", start_ms, end_ms)]);
    assert_eq!(ops.len(), 1);
    let EditOp::Overwrite { at, clip: caption_clip, .. } = &ops[0] else { panic!() };
    assert_eq!(*at, TimeTick(clip.timeline_in.0 + TIMEBASE * 3 / 10));
    assert_eq!(caption_clip.timeline_out.0, clip.timeline_in.0 + TIMEBASE * 6 / 10);
}

#[test]
fn caption_ops_inverts_speed_for_a_retimed_clip() {
    let (mut state, ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let mut clip = state.find_clip(ids[0]).unwrap().1;
    clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
    clip.source_out = TimeTick(clip.source_in.0 + TIMEBASE * 2);
    let ops = state.caption_ops(track, &clip, &[dummy_segment("hi", 0, 1000)]);
    assert_eq!(ops.len(), 1);
    let EditOp::Overwrite { clip: caption_clip, .. } = &ops[0] else { panic!() };
    assert_eq!(caption_clip.timeline_out.0, TIMEBASE / 2, "2x speed should halve the caption's timeline duration");
}

#[test]
fn caption_ops_refuses_a_keyframed_speed_clip() {
    let (mut state, ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let mut clip = state.find_clip(ids[0]).unwrap().1;
    clip.speed = SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0)));
    assert!(state.caption_ops(track, &clip, &[dummy_segment("hi", 0, 500)]).is_empty());
}

#[test]
fn caption_ops_drops_a_zero_length_segment() {
    // A malformed or degenerate segment (start == end) would place a
    // zero-duration clip — Overwrite would reject it anyway, but
    // filtering it here means the whole caption batch doesn't fail for
    // one bad segment.
    let (mut state, ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let clip = state.find_clip(ids[0]).unwrap().1;
    let ops = state.caption_ops(track, &clip, &[dummy_segment("", 100, 100)]);
    assert!(ops.is_empty());
}

#[test]
fn caption_ops_each_get_a_distinct_clip_id() {
    let (mut state, ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let clip = state.find_clip(ids[2]).unwrap().1;
    let ops = state.caption_ops(track, &clip, &[dummy_segment("a", 0, 300), dummy_segment("b", 300, 600)]);
    assert_eq!(ops.len(), 2);
    let mut clip_ids = Vec::new();
    for op in &ops {
        let EditOp::Overwrite { clip, .. } = op else { panic!() };
        clip_ids.push(clip.id);
    }
    assert_ne!(clip_ids[0], clip_ids[1]);
}

/// Runs `generate_captions` against a real file — real audio decode,
/// real local Whisper transcription, real title clips placed on the
/// timeline. Env-gated, `#[ignore]`d, same conventions as the rest of
/// this module. Requires the prebuilt whisper-cli + model to be
/// installed (see `speech::WhisperConfig::default`'s doc).
#[test]
#[ignore]
fn generate_captions_runs_end_to_end_on_real_footage() {
    let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => panic!(
            "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
        ),
    };
    let config = speech::WhisperConfig::default();
    if !config.cli_path.is_file() || !config.model_path.is_file() {
        panic!("whisper-cli or the model isn't installed — see docs/decisions-log.md's speech entry");
    }
    media_ffmpeg::init().unwrap();
    let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
    assert!(asset.audio.is_some(), "this test needs footage with an audio track");
    let video = asset.video.as_ref().expect("expected a video stream");
    let duration = asset.duration_ticks.min(TIMEBASE * 15);

    let mut state = EditorState::new();
    state.asset_paths.insert(asset.id, path);
    let mut project = (**state.project()).clone();
    project.sequences[0].settings.width = video.width;
    project.sequences[0].settings.height = video.height;
    project.assets.push(asset.clone());
    project.sequences[0].tracks.push(Track {
        id: TrackId(1),
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![ClipInstance {
            id: ClipInstanceId(1),
            source: ClipSource::Media(asset.id),
            source_in: TimeTick(0),
            source_out: TimeTick(duration),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(duration),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }],
        transitions: vec![],
        gain_db: timeline::unity_gain(),
        pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
    state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
    state.next_id = 1000;

    let added = state.generate_captions(ClipInstanceId(1));
    println!("real footage captions: {added} caption(s) generated over {:.1}s of source", duration as f64 / TIMEBASE as f64);
    assert!(added > 0, "expected at least one caption from real speech");

    // Captions land on a *different* track than the original video, per
    // `generate_captions`'s "never overwrite existing material" rule.
    assert_eq!(state.sequence().tracks.len(), 2, "expected a new caption track above the video");
    let caption_track = &state.sequence().tracks[1];
    assert_eq!(caption_track.clips.len(), added);
    for c in &caption_track.clips {
        assert!(matches!(c.source, ClipSource::Title(_)), "every caption clip should be a title");
        assert!(c.timeline_out.0 > c.timeline_in.0, "every caption should have real duration");
    }
    let mut sorted: Vec<_> = caption_track.clips.iter().map(|c| (c.timeline_in.0, c.timeline_out.0)).collect();
    sorted.sort();
    for w in sorted.windows(2) {
        assert!(w[0].1 <= w[1].0, "captions must not overlap: {sorted:?}");
    }
}

/// Regression guard for a real bug: `generate_captions` used to write
/// into `self.transcripts` *before* checking the clip's speed curve.
/// Run it on a keyframed-speed clip that already had a real transcript
/// (from an earlier `transcribe_clip` call via the transcript panel),
/// and the correct refusal (0 captions, keyframed speed isn't
/// supported) had the side effect of overwriting that real transcript
/// with an empty one — silent data loss while reporting nothing
/// happened. Real footage + real Whisper, so the bug is only reachable
/// once transcription actually succeeds and reaches the speed check.
#[test]
#[ignore]
fn generate_captions_on_a_keyframed_clip_does_not_clobber_an_existing_transcript() {
    let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => panic!(
            "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
        ),
    };
    let config = speech::WhisperConfig::default();
    if !config.cli_path.is_file() || !config.model_path.is_file() {
        panic!("whisper-cli or the model isn't installed — see docs/decisions-log.md's speech entry");
    }
    media_ffmpeg::init().unwrap();
    let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
    assert!(asset.audio.is_some(), "this test needs footage with an audio track");
    let video = asset.video.as_ref().expect("expected a video stream");
    let duration = asset.duration_ticks.min(TIMEBASE * 15);

    let mut state = EditorState::new();
    state.asset_paths.insert(asset.id, path);
    let mut project = (**state.project()).clone();
    project.sequences[0].settings.width = video.width;
    project.sequences[0].settings.height = video.height;
    project.assets.push(asset.clone());
    project.sequences[0].tracks.push(Track {
        id: TrackId(1),
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![ClipInstance {
            id: ClipInstanceId(1),
            source: ClipSource::Media(asset.id),
            source_in: TimeTick(0),
            source_out: TimeTick(duration),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(duration),
            speed: SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0))),
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }],
        transitions: vec![],
        gain_db: timeline::unity_gain(),
        pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
    state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
    state.next_id = 1000;

    let real_transcript = vec![TimelineWord { text: "hello".into(), start_tick: 0, end_tick: TIMEBASE / 2 }];
    state.store_words(ClipInstanceId(1), real_transcript.clone());

    let added = state.generate_captions(ClipInstanceId(1));

    assert_eq!(added, 0, "keyframed-speed clips aren't supported yet, so no captions should be created");
    assert_eq!(
        words_of(&state, ClipInstanceId(1)),
        Some(real_transcript),
        "the real transcript from the earlier transcribe_clip call must survive the refused caption attempt"
    );
}

// ---- Transcript panel: word mapping and range deletion --------------

#[test]
fn timeline_words_from_transcript_maps_real_words() {
    let (mut state, ids) = state_with_three_clips();
    let clip = state.find_clip(ids[0]).unwrap().1; // 0-1s, source_in=0, 1x
    let segments = vec![speech::Segment {
        text: "hi there".into(),
        start_ms: 0,
        end_ms: 900,
        words: vec![
            speech::Word { text: "hi".into(), start_ms: 100, end_ms: 300 },
            speech::Word { text: "there".into(), start_ms: 300, end_ms: 900 },
        ],
    }];
    let words = state.timeline_words_from_transcript(&clip, &segments);
    assert_eq!(words.len(), 2);
    assert_eq!(words[0], TimelineWord { text: "hi".into(), start_tick: TIMEBASE / 10, end_tick: TIMEBASE * 3 / 10 });
    assert_eq!(words[1].text, "there");
    assert_eq!(words[1].start_tick, TIMEBASE * 3 / 10);
}

#[test]
fn timeline_words_from_transcript_accounts_for_trim_speed_and_bounds() {
    let (mut state, ids) = state_with_three_clips();
    let mut clip = state.find_clip(ids[0]).unwrap().1;
    clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
    clip.source_out = TimeTick(clip.source_in.0 + TIMEBASE * 2);
    let segments = vec![speech::Segment {
        text: "a".into(),
        start_ms: 0,
        end_ms: 2000,
        words: vec![speech::Word { text: "a".into(), start_ms: 1000, end_ms: 2000 }],
    }];
    let words = state.timeline_words_from_transcript(&clip, &segments);
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].start_tick, TIMEBASE / 2, "2x speed should halve the timeline offset");
    assert_eq!(words[0].end_tick, TIMEBASE);
}

#[test]
fn timeline_range_removal_ops_produces_two_razors_and_one_extract() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let ops = state.timeline_range_removal_ops(track, TIMEBASE / 4, TIMEBASE / 2);
    assert_eq!(ops.len(), 3);
    assert!(matches!(ops[2], EditOp::Extract { .. }));
    let EditOp::Razor { at: at0, .. } = ops[0] else { panic!() };
    let EditOp::Razor { at: at1, .. } = ops[1] else { panic!() };
    assert_eq!(at0, TimeTick(TIMEBASE / 4));
    assert_eq!(at1, TimeTick(TIMEBASE / 2));
}

#[test]
fn timeline_range_removal_ops_rejects_an_inverted_or_empty_range() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    assert!(state.timeline_range_removal_ops(track, TIMEBASE / 2, TIMEBASE / 2).is_empty());
    assert!(state.timeline_range_removal_ops(track, TIMEBASE / 2, TIMEBASE / 4).is_empty());
}

#[test]
fn delete_word_range_ripple_deletes_the_spanned_words_timeline_range() {
    // An *interior* selection deliberately — "two" alone, not touching
    // either edge of the clip. `delete_word_range` is scoped to interior
    // ranges only, same as `silence_removal_ops`: a range touching an
    // edge is a trim (shorten the clip), not an isolate-and-extract, and
    // handling both shapes in one action is real, separate follow-up
    // work rather than something to half-solve here.
    let (mut state, ids) = state_with_three_clips();
    let clip_id = ids[0]; // spans [0, TIMEBASE)
    state.store_words(
        clip_id,
        vec![
            TimelineWord { text: "one".into(), start_tick: 0, end_tick: TIMEBASE / 5 },
            TimelineWord { text: "two".into(), start_tick: TIMEBASE / 5, end_tick: TIMEBASE * 2 / 5 },
            TimelineWord { text: "three".into(), start_tick: TIMEBASE * 2 / 5, end_tick: TIMEBASE },
        ],
    );
    let track = state.sequence().tracks[0].id;
    let removed = state.delete_word_range(track, clip_id, 1, 1); // "two" only
    assert!(removed, "should successfully ripple-delete the selected word");
    // `state_with_three_clips` already has two more clips on this same
    // track (originally at 1-2s and 2-3s), untouched apart from
    // rippling left. Clip 0 itself nets to *two* remaining pieces — cut
    // a chunk out of the middle of one clip and what's left is a head
    // and a tail, not three — so four clips total, not the fixture's
    // original three.
    let clips = &state.sequence().tracks[0].clips;
    assert_eq!(clips.len(), 4, "clip 0's head + tail, plus the two untouched (rippled) clips after it");
    let mut sorted: Vec<_> = clips.iter().map(|c| (c.timeline_in.0, c.timeline_out.0)).collect();
    sorted.sort();
    assert_eq!(sorted[0], (0, TIMEBASE / 5), "\"one\" keeps its start and shrinks to just before \"two\"");
    // "three" started at 2/5 and the deletion removed 1/5 of duration, so
    // the tail piece now starts right where "one" ends.
    assert_eq!(sorted[1].0, TIMEBASE / 5, "\"three\" ripples left to close the gap \"two\" left");
    for w in sorted.windows(2) {
        assert_eq!(w[0].1, w[1].0, "every piece must still abut with no gap or overlap: {sorted:?}");
    }
}

#[test]
fn delete_word_range_returns_false_for_an_edge_touching_selection() {
    let (mut state, ids) = state_with_three_clips();
    let clip_id = ids[0];
    state.store_words(
        clip_id,
        vec![TimelineWord { text: "one".into(), start_tick: 0, end_tick: TIMEBASE / 2 }],
    );
    let track = state.sequence().tracks[0].id;
    assert!(!state.delete_word_range(track, clip_id, 0, 0), "a selection touching the clip's own start must be refused");
}

#[test]
fn delete_word_range_never_reaches_into_the_next_clip_on_the_track() {
    // The case that actually needs the bounds guard, not just a "touches
    // the edge exactly" formality: a word whose end tick has drifted
    // past this clip's own end (e.g. a stale transcript after the clip
    // was trimmed) must never let the deletion razor into — and
    // ripple-delete part of — a completely different, unrelated clip
    // sitting right after it on the same track.
    let (mut state, ids) = state_with_three_clips(); // clip0 [0,1s), clip1 [1s,2s)
    let clip_id = ids[0];
    state.store_words(
        clip_id,
        vec![TimelineWord { text: "one".into(), start_tick: TIMEBASE / 10, end_tick: TIMEBASE + TIMEBASE / 10 }],
    );
    let track = state.sequence().tracks[0].id;
    let before = state.project().clone();
    assert!(!state.delete_word_range(track, clip_id, 0, 0), "a word extending past this clip's own end must be refused");
    assert_eq!(**state.project(), *before, "a refused deletion must leave the project completely untouched");
}

#[test]
fn delete_word_range_returns_false_for_an_out_of_range_selection() {
    let (mut state, ids) = state_with_three_clips();
    let clip_id = ids[0];
    state.store_words(clip_id, vec![TimelineWord { text: "one".into(), start_tick: 0, end_tick: TIMEBASE / 2 }]);
    let track = state.sequence().tracks[0].id;
    assert!(!state.delete_word_range(track, clip_id, 5, 9));
}

#[test]
fn moving_a_clip_makes_its_transcript_stale_instead_of_silently_wrong() {
    // Word ticks are derived from where the clip sat when it was
    // transcribed, and nothing recomputes them afterwards. Left unchecked
    // the panel would keep offering those words: clicking one seeks to
    // where it used to be, and deleting a run ripple-deletes whatever now
    // occupies that span. `Placement` already encodes "this result no
    // longer describes this clip" for in-flight analyses; a stored
    // transcript is the same claim, just cached for longer.
    let (mut state, ids) = state_with_three_clips();
    let clip_id = ids[0];
    let track = state.sequence().tracks[0].id;
    state.store_words(
        clip_id,
        vec![
            TimelineWord { text: "one".into(), start_tick: 0, end_tick: TIMEBASE / 5 },
            TimelineWord { text: "two".into(), start_tick: TIMEBASE / 5, end_tick: TIMEBASE * 2 / 5 },
            TimelineWord { text: "three".into(), start_tick: TIMEBASE * 2 / 5, end_tick: TIMEBASE },
        ],
    );
    assert!(matches!(state.transcript(clip_id), Transcript::Ready(_)));

    // Out to empty timeline past the other two, so the move is
    // unobstructed and the only thing that changed is where this clip sits.
    state.begin_drag_edit("move clip");
    state.move_clip(clip_id, track, TIMEBASE * 10);
    state.end_drag_edit();

    assert!(
        matches!(state.transcript(clip_id), Transcript::Stale),
        "a moved clip's transcript must report itself out of date"
    );
    let before = state.project().clone();
    assert!(
        !state.delete_word_range(track, clip_id, 1, 1),
        "and must not be usable to cut footage at ticks it no longer describes"
    );
    assert_eq!(**state.project(), *before, "a refused deletion must leave the project untouched");
}

#[test]
fn re_transcribing_a_moved_clip_makes_its_transcript_usable_again() {
    // The other half: staleness has to be recoverable, or the panel's
    // "transcribe again" is a dead end.
    let (mut state, ids) = state_with_three_clips();
    let clip_id = ids[0];
    state.store_words(clip_id, vec![TimelineWord { text: "one".into(), start_tick: 0, end_tick: TIMEBASE / 5 }]);
    let track = state.sequence().tracks[0].id;
    state.begin_drag_edit("move clip");
    state.move_clip(clip_id, track, TIMEBASE * 10);
    state.end_drag_edit();
    assert!(matches!(state.transcript(clip_id), Transcript::Stale));

    state.store_words(clip_id, vec![TimelineWord { text: "one".into(), start_tick: TIMEBASE * 10, end_tick: TIMEBASE * 10 + TIMEBASE / 5 }]);

    assert!(matches!(state.transcript(clip_id), Transcript::Ready(_)));
}

/// Runs `detect_scene_cuts` against a real file, decoding real frames
/// through the real `media_ffmpeg` path — the integration risk the pure
/// `scene_cut_ops` tests above can't reach. Env-gated and `#[ignore]`d,
/// matching `crates/playback/tests/real_footage_smoke.rs`'s convention:
/// real footage is machine-specific and not something to commit or run
/// in ordinary `cargo test`.
///
/// The assertion is deliberately loose (runs to completion, returns a
/// plausible count) rather than pinned to an exact number of cuts: unlike
/// the synthetic tests above, this file's true cut count isn't known
/// ahead of time, and a screen recording may have few or none at all.
/// What this test actually proves is that decoding, histogramming, and
/// applying real `Razor` ops against a real H.264 file works end to end
/// without silently breaking — the pure tests already prove the
/// detection math itself is correct.
#[test]
#[ignore]
fn detect_scene_cuts_runs_end_to_end_on_real_footage() {
    let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => panic!(
            "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
        ),
    };
    media_ffmpeg::init().unwrap();
    let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
    let video = asset.video.as_ref().expect("expected a video stream");
    let duration = asset.duration_ticks.min(TIMEBASE * 20); // cap the sample at 20s of source

    let mut state = EditorState::new();
    state.asset_paths.insert(asset.id, path);
    let mut project = (**state.project()).clone();
    let track_id = TrackId(1);
    project.sequences[0].settings.width = video.width;
    project.sequences[0].settings.height = video.height;
    project.assets.push(asset.clone());
    project.sequences[0].tracks.push(Track {
        id: track_id,
        kind: TrackKind::Video,
        name: "V1".into(),
        clips: vec![ClipInstance {
            id: ClipInstanceId(1),
            source: ClipSource::Media(asset.id),
            source_in: TimeTick(0),
            source_out: TimeTick(duration),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(duration),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }],
        transitions: vec![],
        gain_db: timeline::unity_gain(),
        pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
    state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
    // The fixture above hand-picks ClipInstanceId(1) directly rather than
    // going through `next_id()` (which a real import/add path always
    // does) — so `next_id` must be advanced past it here, or the method
    // under test would allocate a colliding id the moment it calls
    // `self.next_id()` itself, corrupting the very project it's editing.
    state.next_id = 1000;

    let found = state.detect_scene_cuts(ClipInstanceId(1));
    println!(
        "real footage scene-cut detection: {found} cut(s) found over {:.1}s of source",
        duration as f64 / TIMEBASE as f64
    );

    // The clip should have been split into `found + 1` pieces on the
    // track, and every resulting clip must still be a real, non-empty,
    // non-overlapping span — `check_no_overlaps`-style sanity, proving
    // the applied `Razor` ops actually left a valid project rather than
    // merely not panicking.
    let clips = &state.sequence().tracks[0].clips;
    assert_eq!(clips.len(), found + 1, "expected one more clip than cuts found");
    let mut sorted: Vec<_> = clips.iter().map(|c| (c.timeline_in.0, c.timeline_out.0)).collect();
    sorted.sort();
    for w in sorted.windows(2) {
        assert_eq!(w[0].1, w[1].0, "split pieces must abut with no gap or overlap: {sorted:?}");
    }
    let mut ids: Vec<_> = clips.iter().map(|c| c.id).collect();
    ids.sort_by_key(|id| id.0);
    ids.dedup();
    assert_eq!(ids.len(), clips.len(), "every resulting piece must have a distinct id");
    assert!(found < 160, "8 samples/sec over 20s caps at 160 samples — can't exceed that many cuts");
}

/// Three abutting 1s clips on one video track: 0-1s, 1-2s, 2-3s. Abutting
/// on purpose — it's the layout where lift vs ripple delete actually differ.
fn state_with_three_clips() -> (EditorState, Vec<ClipInstanceId>) {
    let mut state = EditorState::new();
    let mut project = (**state.project()).clone();
    let ids: Vec<ClipInstanceId> = (1..=3).map(|i| ClipInstanceId(i * 10)).collect();
    let clips: Vec<ClipInstance> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let start = i as i64 * TIMEBASE;
            ClipInstance {
                id: *id,
                source: ClipSource::Media(media::MediaAssetId(1)),
                source_in: TimeTick(0),
                source_out: TimeTick(TIMEBASE),
                timeline_in: TimeTick(start),
                timeline_out: TimeTick(start + TIMEBASE),
                speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
                effects: vec![],
                audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                linked_group: None,
            }
        })
        .collect();
    project.sequences[0].tracks.push(Track {
        id: TrackId(1),
        kind: TrackKind::Video,
        name: "V1".into(),
        clips,
        transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
    project.assets.push(fake_asset(1, "C:/media/clip.mp4"));
    state.undo.push("setup", std::sync::Arc::new(project));
    state.selected_clips.clear();
    (state, ids)
}

// ---- Background analysis: applying finished jobs --------------------------

fn placement_now(state: &EditorState, clip: ClipInstanceId) -> analysis_jobs::Placement {
    let (track, instance) = state.find_clip(clip).expect("fixture clip exists");
    analysis_jobs::Placement::of(track, &instance)
}

/// Queues a result as if a worker had just finished computing it against
/// `placement`.
fn finish_analysis(
    state: &mut EditorState,
    clip: ClipInstanceId,
    kind: AnalysisKind,
    placement: analysis_jobs::Placement,
    result: Result<analysis_jobs::Outcome, analysis_jobs::AnalysisError>,
) {
    state.analysis.inject_for_test(analysis_jobs::Finished { clip_id: clip, kind, placement, result });
}

#[test]
fn a_finished_analysis_is_applied_when_its_clip_is_unchanged() {
    let (mut state, ids) = state_with_three_clips();
    let placement = placement_now(&state, ids[1]);
    finish_analysis(
        &mut state,
        ids[1],
        AnalysisKind::SceneCuts,
        placement,
        Ok(analysis_jobs::Outcome::SceneCuts(vec![TIMEBASE / 2])),
    );

    state.poll_analysis();

    assert_eq!(state.sequence().tracks[0].clips.len(), 4, "the cut should split the middle clip");
    assert_eq!(clip_starts(&state), vec![0, TIMEBASE, TIMEBASE + TIMEBASE / 2, 2 * TIMEBASE]);
    assert_eq!(state.status, "found and split at 1 scene cut");
    assert!(!state.analysis_running(ids[1], AnalysisKind::SceneCuts));
}

/// The case the whole `Placement` check exists for: the cut positions were
/// computed against where the clip *was*, so applying them after a move
/// would razor some other part of the timeline.
#[test]
fn a_finished_analysis_is_discarded_if_its_clip_moved_while_it_ran() {
    let (mut state, ids) = state_with_three_clips();
    let placement = placement_now(&state, ids[2]);
    state.begin_drag_edit("move clip");
    state.move_clip(ids[2], TrackId(1), 5 * TIMEBASE);
    state.end_drag_edit();
    assert_ne!(placement_now(&state, ids[2]), placement, "setup: the clip should have moved");
    let before = (**state.project()).clone();
    finish_analysis(
        &mut state,
        ids[2],
        AnalysisKind::SceneCuts,
        placement,
        Ok(analysis_jobs::Outcome::SceneCuts(vec![TIMEBASE / 2])),
    );

    state.poll_analysis();

    assert_eq!(**state.project(), before, "a stale result must not edit the project");
    assert!(state.status.contains("run it again"), "status should say why nothing happened: {:?}", state.status);
}

#[test]
fn a_finished_analysis_is_discarded_if_its_clip_was_deleted_while_it_ran() {
    let (mut state, ids) = state_with_three_clips();
    let placement = placement_now(&state, ids[2]);
    assert!(state.apply_op("delete", EditOp::Extract { clip: ids[2] }));
    let before = (**state.project()).clone();
    finish_analysis(
        &mut state,
        ids[2],
        AnalysisKind::Silence,
        placement,
        Ok(analysis_jobs::Outcome::Silence(vec![(0.2, 0.6)])),
    );

    state.poll_analysis();

    assert_eq!(**state.project(), before);
    assert!(state.status.contains("deleted"), "status should say why nothing happened: {:?}", state.status);
}

/// Discarding on *any* edit would make these actions useless on a long clip:
/// nobody sits idle for minutes. Only edits to what the result was computed
/// against should invalidate it.
#[test]
fn a_finished_analysis_still_applies_after_an_unrelated_edit_to_the_same_clip() {
    let (mut state, ids) = state_with_three_clips();
    let placement = placement_now(&state, ids[0]);
    assert!(state.apply_loudness_match_gain(ids[0], -3.0), "setup: change the clip's gain meanwhile");
    finish_analysis(
        &mut state,
        ids[0],
        AnalysisKind::Beats,
        placement,
        Ok(analysis_jobs::Outcome::Beats(vec![0.5])),
    );

    state.poll_analysis();

    assert_eq!(state.sequence().markers.len(), 1);
    assert_eq!(state.sequence().markers[0].position, TimeTick(TIMEBASE / 2));
    assert_eq!(state.status, "added 1 beat marker");
}

#[test]
fn a_failed_analysis_reports_why_and_leaves_the_project_alone() {
    let (mut state, ids) = state_with_three_clips();
    let before = (**state.project()).clone();
    let placement = placement_now(&state, ids[0]);
    finish_analysis(&mut state, ids[0], AnalysisKind::Beats, placement, Err(analysis_jobs::AnalysisError::NoAudio));

    state.poll_analysis();

    assert_eq!(**state.project(), before);
    assert_eq!(state.status, "couldn't detect beats — this clip has no audio to analyse");
}

#[test]
fn a_finished_loudness_measurement_sets_the_clips_gain() {
    let (mut state, ids) = state_with_three_clips();
    let placement = placement_now(&state, ids[0]);
    finish_analysis(
        &mut state,
        ids[0],
        AnalysisKind::Loudness,
        placement,
        Ok(analysis_jobs::Outcome::Loudness { gain_db: -6.0, target_lufs: -14.0 }),
    );

    state.poll_analysis();

    let (_, clip) = state.find_clip(ids[0]).unwrap();
    assert_eq!(clip.audio_gain_db.default, ParamValue::Number(-6.0));
    assert_eq!(state.status, "applied -6.0dB to reach -14 LUFS");
}

/// A result for clip 20 of the *previous* project must not razor clip 20 of
/// the one just opened — ids restart per project, so they collide routinely.
#[test]
fn analysis_still_running_when_a_project_is_opened_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.nleproj");
    let (mut state, ids) = state_with_three_clips();
    state.save_to(&path);
    let placement = placement_now(&state, ids[1]);
    finish_analysis(
        &mut state,
        ids[1],
        AnalysisKind::SceneCuts,
        placement,
        Ok(analysis_jobs::Outcome::SceneCuts(vec![TIMEBASE / 2])),
    );

    assert!(state.open_from(&path));
    state.poll_analysis();

    assert_eq!(state.sequence().tracks[0].clips.len(), 3, "the old project's result must not apply");
    assert!(!state.analysis_running(ids[1], AnalysisKind::SceneCuts));
}

#[test]
fn starting_a_speed_dependent_analysis_on_a_keyframed_clip_is_refused_up_front() {
    let (mut state, ids) = state_with_three_clips();
    let mut project = (**state.project()).clone();
    project.sequences[0].tracks[0].clips[0].speed =
        SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0)));
    state.undo.push("keyframe speed", std::sync::Arc::new(project));

    assert!(!state.start_analysis(ids[0], AnalysisKind::SceneCuts, -14.0));

    assert!(!state.analysis_running(ids[0], AnalysisKind::SceneCuts), "no work should have been started");
    assert!(state.status.contains("constant speed"), "status should say why: {:?}", state.status);
}

/// A project file is untrusted input, and nothing validates clip speeds when
/// one is opened. A zero numerator used to reach `* denominator / numerator`
/// in the analysis apply helpers — on the UI thread, so the editor exited.
fn state_with_zero_speed_first_clip() -> (EditorState, Vec<ClipInstanceId>) {
    let (mut state, ids) = state_with_three_clips();
    let mut project = (**state.project()).clone();
    project.sequences[0].tracks[0].clips[0].speed = SpeedCurve::Constant { numerator: 0, denominator: 1 };
    state.undo.push("crafted speed", std::sync::Arc::new(project));
    state.next_id = 1000;
    (state, ids)
}

#[test]
fn finished_results_for_a_zero_speed_clip_are_refused_instead_of_crashing() {
    let (mut state, ids) = state_with_zero_speed_first_clip();
    let before = (**state.project()).clone();
    let placement = placement_now(&state, ids[0]);
    let segments = vec![speech::Segment {
        text: "hi".into(),
        start_ms: 100,
        end_ms: 400,
        words: vec![speech::Word { text: "hi".into(), start_ms: 100, end_ms: 400 }],
    }];
    let outcomes = [
        (AnalysisKind::SceneCuts, analysis_jobs::Outcome::SceneCuts(vec![TIMEBASE / 2])),
        (AnalysisKind::Silence, analysis_jobs::Outcome::Silence(vec![(0.2, 0.6)])),
        (AnalysisKind::Beats, analysis_jobs::Outcome::Beats(vec![0.5])),
        (AnalysisKind::Captions, analysis_jobs::Outcome::Captions(segments.clone())),
        (AnalysisKind::Transcribe, analysis_jobs::Outcome::Transcribe(segments)),
    ];
    for (kind, outcome) in outcomes {
        finish_analysis(&mut state, ids[0], kind, placement.clone(), Ok(outcome));
    }

    state.poll_analysis();

    assert_eq!(**state.project(), before, "nothing computed against a zero speed may be applied");
    assert!(
        words_of(&state, ids[0]).is_none_or(|w| w.is_empty()),
        "no words can be placed on the timeline at zero speed"
    );
}

#[test]
fn starting_an_analysis_on_a_zero_or_negative_speed_clip_is_refused_up_front() {
    for (numerator, denominator) in [(0, 1), (-1, 1), (1, 0), (1, -2)] {
        let (mut state, ids) = state_with_three_clips();
        let mut project = (**state.project()).clone();
        project.sequences[0].tracks[0].clips[0].speed = SpeedCurve::Constant { numerator, denominator };
        state.undo.push("crafted speed", std::sync::Arc::new(project));

        assert!(
            !state.start_analysis(ids[0], AnalysisKind::SceneCuts, -14.0),
            "a {numerator}/{denominator} speed must be refused before any work starts"
        );
        assert!(state.status.contains("constant speed"), "status should say why: {:?}", state.status);
    }
}

/// Stabilize divides on the worker thread, where a panic is caught and
/// reported rather than crashing — but it should never get that far.
#[test]
fn stabilizing_a_zero_speed_clip_returns_nothing_instead_of_dividing_by_zero() {
    media_ffmpeg::init().unwrap();
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test_fixtures/test_h264.mp4");
    let (state, ids) = state_with_zero_speed_first_clip();
    let (_, clip) = state.find_clip(ids[0]).unwrap();
    let ClipSource::Media(asset) = clip.source else { panic!("the fixture clip is media") };
    let paths = std::collections::HashMap::from([(asset, fixture)]);

    let keyframes = super::analysis::stabilization_keyframes(&paths, &clip).expect("the fixture decodes");

    assert!(keyframes.is_empty());
}

#[test]
fn finished_captions_land_on_a_new_track_and_fill_the_transcript() {
    let (mut state, ids) = state_with_three_clips();
    // The fixture hand-picks its ids without advancing `next_id`, so the new
    // caption track would otherwise be allocated V1's own id.
    state.next_id = 1000;
    let placement = placement_now(&state, ids[0]);
    let segments = vec![speech::Segment {
        text: "hi there".into(),
        start_ms: 100,
        end_ms: 900,
        words: vec![
            speech::Word { text: "hi".into(), start_ms: 100, end_ms: 300 },
            speech::Word { text: "there".into(), start_ms: 300, end_ms: 900 },
        ],
    }];
    finish_analysis(&mut state, ids[0], AnalysisKind::Captions, placement, Ok(analysis_jobs::Outcome::Captions(segments)));

    state.poll_analysis();

    assert_eq!(state.sequence().tracks.len(), 2, "V1 is occupied there, so captions need a track above");
    let captions = &state.sequence().tracks[1].clips;
    assert_eq!(captions.len(), 1);
    assert!(matches!(captions[0].source, ClipSource::Title(_)));
    assert_eq!(words_of(&state, ids[0]).map(|w| w.len()), Some(2));
    assert_eq!(state.status, "generated 1 caption");
}

#[test]
fn a_finished_transcription_is_stored_without_touching_the_timeline() {
    let (mut state, ids) = state_with_three_clips();
    let before = (**state.project()).clone();
    let placement = placement_now(&state, ids[0]);
    let segments = vec![dummy_segment("", 0, 900)];
    let mut with_words = segments.clone();
    with_words[0].words = vec![speech::Word { text: "hello".into(), start_ms: 100, end_ms: 400 }];
    finish_analysis(&mut state, ids[0], AnalysisKind::Transcribe, placement, Ok(analysis_jobs::Outcome::Transcribe(with_words)));

    state.poll_analysis();

    assert_eq!(**state.project(), before);
    assert_eq!(
        words_of(&state, ids[0]),
        Some(vec![TimelineWord { text: "hello".into(), start_tick: TIMEBASE / 10, end_tick: TIMEBASE * 4 / 10 }])
    );
    assert_eq!(state.status, "transcribed 1 word");
}

/// End to end through a real worker thread: the click returns immediately,
/// a second click while it runs is refused, and the failure (this fixture's
/// media file doesn't exist) arrives through `poll_analysis`.
#[test]
fn start_analysis_runs_in_the_background_and_reports_back_through_poll() {
    let (mut state, ids) = state_with_three_clips();

    assert!(state.start_analysis(ids[0], AnalysisKind::SceneCuts, -14.0));
    assert!(state.analysis_running(ids[0], AnalysisKind::SceneCuts));
    assert!(!state.start_analysis(ids[0], AnalysisKind::SceneCuts, -14.0), "a duplicate must be refused");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while state.analysis_running(ids[0], AnalysisKind::SceneCuts) && std::time::Instant::now() < deadline {
        state.poll_analysis();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert!(!state.analysis_running(ids[0], AnalysisKind::SceneCuts), "the job never reported back");
    assert_eq!(state.status, "couldn't detect scene cuts — the clip's media file can't be found");
    assert_eq!(state.sequence().tracks[0].clips.len(), 3);
}

/// Live bug (2026-09-11): running Remove Silence to completion while Detect
/// Scene Cuts was still going on the same clip replaced "Detecting scene
/// cuts…" in the status line with "removed 47 silent gaps" — making the
/// still-running job look like it had vanished. The button already shows
/// `kind.running_label()` in place of itself for as long as the job runs
/// (see `analysis_button`), so `start_analysis` writing the same text into
/// the shared status line was pure redundancy — and the one thing with
/// nothing else competing to overwrite it wasn't the button, it was this
/// field. Fix: starting a job no longer touches `status` at all.
#[test]
fn starting_an_analysis_does_not_touch_the_status_line() {
    let (mut state, ids) = state_with_three_clips();
    state.status = "an earlier, unrelated message".into();

    assert!(state.start_analysis(ids[0], AnalysisKind::SceneCuts, -14.0));

    assert_eq!(
        state.status, "an earlier, unrelated message",
        "the button shows the running label; the shared status line must be left for one-shot events only"
    );
}

fn clip_starts(state: &EditorState) -> Vec<i64> {
    let mut v: Vec<i64> = state.sequence().tracks[0]
        .clips
        .iter()
        .map(|c| c.timeline_in.0)
        .collect();
    v.sort_unstable();
    v
}

fn transitions_of(state: &EditorState) -> Vec<timeline::Transition> {
    state.sequence().tracks[0].transitions.clone()
}

#[test]
fn a_transition_lands_on_the_cut_and_is_undoable() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let cut = TimeTick(TIMEBASE); // between clip 1 and clip 2

    assert!(state.add_transition(
        track,
        cut,
        timeline::TransitionKind::CrossDissolve,
        TimeTick(TIMEBASE / 2),
    ));
    let trs = transitions_of(&state);
    assert_eq!(trs.len(), 1);
    assert_eq!(trs[0].at, cut);
    assert_eq!(trs[0].duration, TimeTick(TIMEBASE / 2));
    // Centred on the cut.
    assert_eq!(trs[0].region(), (TimeTick(TIMEBASE * 3 / 4), TimeTick(TIMEBASE * 5 / 4)));

    state.undo.undo();
    assert!(transitions_of(&state).is_empty());
}

#[test]
fn a_transition_is_clamped_to_fit_its_neighbouring_clips() {
    // Unclamped, a transition wider than its clips reaches into material
    // that isn't in the sequence, and both halves show frozen frames
    // instead of a dissolve.
    let (mut state, _) = state_with_three_clips(); // 1s clips
    let track = state.sequence().tracks[0].id;

    state.add_transition(
        track,
        TimeTick(TIMEBASE),
        timeline::TransitionKind::CrossDissolve,
        TimeTick(TIMEBASE * 10), // absurdly long
    );
    let tr = &transitions_of(&state)[0];
    assert_eq!(
        tr.duration,
        TimeTick(TIMEBASE * 2),
        "each half must fit inside its 1s clip, so 2s total is the maximum"
    );
    assert!(state.status.contains("shortened"), "the clamp should be reported, not silent");
}

#[test]
fn adding_a_transition_twice_on_one_cut_replaces_it() {
    // Two transitions claiming the same ticks would make `transition_at`'s
    // "first match wins" arbitrary.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let cut = TimeTick(TIMEBASE);

    state.add_transition(track, cut, timeline::TransitionKind::CrossDissolve, TimeTick(1000));
    state.add_transition(track, cut, timeline::TransitionKind::DipToBlack, TimeTick(2000));

    let trs = transitions_of(&state);
    assert_eq!(trs.len(), 1, "the second add must replace, not stack");
    assert_eq!(trs[0].kind, timeline::TransitionKind::DipToBlack);
    assert_eq!(trs[0].duration, TimeTick(2000));
}

// ---- Titles -------------------------------------------------------

fn only_title(state: &EditorState) -> timeline::TitleSpec {
    let clip = state
        .sequence()
        .tracks
        .iter()
        .flat_map(|t| &t.clips)
        .find(|c| matches!(c.source, ClipSource::Title(_)))
        .expect("expected a title clip");
    let ClipSource::Title(spec) = &clip.source else { unreachable!() };
    spec.clone()
}

#[test]
fn adding_a_title_puts_a_title_clip_on_the_track_at_the_playhead() {
    let (mut state, _) = state_with_three_clips();
    // Past the end of the three media clips (which span 0..3s), so the
    // title lands on empty track rather than overwriting them.
    let track = state.sequence().tracks[0].id;
    state.playhead = TIMEBASE * 5;

    let id = state.add_title(track, TimeTick(TIMEBASE * 2)).expect("title added");

    let clip = state
        .sequence()
        .tracks
        .iter()
        .flat_map(|t| &t.clips)
        .find(|c| c.id == id)
        .expect("the new clip exists");
    assert_eq!(clip.timeline_in, TimeTick(TIMEBASE * 5), "starts at the playhead");
    assert_eq!(clip.timeline_out, TimeTick(TIMEBASE * 7), "and runs for the given duration");
    assert!(matches!(clip.source, ClipSource::Title(_)));
}

#[test]
fn a_new_title_is_selected_so_it_can_be_typed_into_immediately() {
    // Adding a title and then having to hunt for it before typing would be
    // a poor enough gesture that the feature wouldn't get used.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.playhead = TIMEBASE * 5;
    let id = state.add_title(track, TimeTick(TIMEBASE)).expect("title added");
    assert_eq!(state.selected_clips, vec![id]);
}

#[test]
fn editing_a_titles_text_is_undoable() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.playhead = TIMEBASE * 5;
    let id = state.add_title(track, TimeTick(TIMEBASE)).expect("title added");

    let mut spec = only_title(&state);
    spec.text = "Rewritten".into();
    assert!(state.set_title_spec(id, spec));
    assert_eq!(only_title(&state).text, "Rewritten");

    state.undo.undo();
    assert_eq!(
        only_title(&state).text,
        timeline::TitleSpec::default().text,
        "undo should restore the title's previous text, not remove the clip"
    );
}

#[test]
fn consecutive_keystrokes_in_one_title_coalesce_into_one_undo_step() {
    // Typing a word is one edit to a human. Without coalescing, undoing a
    // title would walk back one character at a time — the same reason
    // dragging a clip is one undo step and not 400.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.playhead = TIMEBASE * 5;
    let id = state.add_title(track, TimeTick(TIMEBASE)).expect("title added");
    let depth_before = state.undo.history().len();

    for text in ["H", "He", "Hel", "Hell", "Hello"] {
        let mut spec = only_title(&state);
        spec.text = text.into();
        state.set_title_spec(id, spec);
    }

    assert_eq!(
        state.undo.history().len(),
        depth_before + 1,
        "five keystrokes should be one undo step, not five"
    );
    state.undo.undo();
    assert_eq!(only_title(&state).text, timeline::TitleSpec::default().text);
}

#[test]
fn setting_a_title_spec_that_changes_nothing_records_no_undo_step() {
    // The properties panel re-submits the spec every frame it's shown.
    // Pushing an undo entry for each would fill the whole history with
    // no-ops within seconds of selecting a title.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.playhead = TIMEBASE * 5;
    let id = state.add_title(track, TimeTick(TIMEBASE)).expect("title added");
    let depth = state.undo.history().len();

    let unchanged = only_title(&state);
    assert!(!state.set_title_spec(id, unchanged), "an identical spec is not an edit");
    assert_eq!(state.undo.history().len(), depth);
}

#[test]
fn add_title_at_playhead_uses_the_top_video_track_when_it_is_free_there() {
    let (mut state, _) = state_with_three_clips();
    let existing_tracks = state.sequence().tracks.len();
    // Past the three clips, so the top video track is empty here.
    state.playhead = TIMEBASE * 10;

    state.add_title_at_playhead(TimeTick(TIMEBASE)).expect("title added");

    assert_eq!(
        state.sequence().tracks.len(),
        existing_tracks,
        "no need for a new track when the top one is free"
    );
    assert_eq!(only_title(&state), timeline::TitleSpec::default());
}

#[test]
fn add_title_at_playhead_makes_a_new_track_rather_than_overwriting_footage() {
    // The important one. A title inserted over existing video must not
    // destroy it — "Add Title" is a create gesture, and silently eating a
    // second of footage would be the worst kind of data loss: invisible
    // until you scrub back to it.
    let (mut state, ids) = state_with_three_clips();
    let existing_tracks = state.sequence().tracks.len();
    // Squarely on top of the middle clip.
    state.playhead = TIMEBASE + TIMEBASE / 2;

    state.add_title_at_playhead(TimeTick(TIMEBASE)).expect("title added");

    assert_eq!(
        state.sequence().tracks.len(),
        existing_tracks + 1,
        "an occupied top track should get a new one above it"
    );
    let media_clips: Vec<ClipInstanceId> = state
        .sequence()
        .tracks
        .iter()
        .flat_map(|t| &t.clips)
        .filter(|c| matches!(c.source, ClipSource::Media(_)))
        .map(|c| c.id)
        .collect();
    assert_eq!(media_clips, ids, "every original clip must survive untouched");
}

#[test]
fn adding_a_title_over_footage_is_a_single_undo_step() {
    // Creating the track and placing the clip are one gesture to the user,
    // so one Ctrl+Z must put things back exactly as they were.
    let (mut state, _) = state_with_three_clips();
    let before = state.project().clone();
    state.playhead = TIMEBASE + TIMEBASE / 2;

    state.add_title_at_playhead(TimeTick(TIMEBASE)).expect("title added");
    state.undo.undo();

    assert_eq!(**state.project(), *before, "one undo restores the whole gesture");
}

#[test]
fn set_title_spec_on_a_clip_that_is_not_a_title_does_nothing() {
    let (mut state, ids) = state_with_three_clips();
    let depth = state.undo.history().len();
    assert!(!state.set_title_spec(ids[0], timeline::TitleSpec::default()));
    assert_eq!(state.undo.history().len(), depth);
}

#[test]
fn a_transition_needs_an_actual_cut() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    // Mid-clip: no clip starts or ends here.
    assert!(!state.add_transition(
        track,
        TimeTick(TIMEBASE / 2),
        timeline::TransitionKind::CrossDissolve,
        TimeTick(1000),
    ));
    assert!(transitions_of(&state).is_empty());
    assert!(!state.status.is_empty(), "should say why nothing happened");
}

#[test]
fn removing_a_transition_leaves_the_clips_alone() {
    let (mut state, ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let cut = TimeTick(TIMEBASE);
    state.add_transition(track, cut, timeline::TransitionKind::CrossDissolve, TimeTick(1000));

    state.remove_transition(track, cut);

    assert!(transitions_of(&state).is_empty());
    assert_eq!(state.sequence().tracks[0].clips.len(), 3);
    assert_eq!(clip_starts(&state), vec![0, TIMEBASE, TIMEBASE * 2]);
    assert!(state.find_clip(ids[0]).is_some(), "clips must be untouched");
}

#[test]
fn transitions_survive_a_save_load_round_trip() {
    // The schema v3 field has to actually persist — a transition that
    // vanishes on reopen is worse than one that never existed.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.add_transition(
        track,
        TimeTick(TIMEBASE),
        timeline::TransitionKind::DipToBlack,
        TimeTick(TIMEBASE / 2),
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.nleproj");
    state.save_to(&path);

    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&path));
    let trs = transitions_of(&fresh);
    assert_eq!(trs.len(), 1);
    assert_eq!(trs[0].kind, timeline::TransitionKind::DipToBlack);
    assert_eq!(trs[0].at, TimeTick(TIMEBASE));
}

#[test]
fn a_track_fader_and_pan_are_stored_and_undoable() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;

    state.set_track_gain(track, -9.0, false);
    state.set_track_pan(track, -0.5);
    let t = &state.sequence().tracks[0];
    assert_eq!(t.gain_db.evaluate_at(TimeTick(0)).as_scalar(), Some(-9.0));
    assert_eq!(t.pan, -0.5);

    state.undo.undo(); // pan
    assert_eq!(state.sequence().tracks[0].pan, 0.0);
    state.undo.undo(); // fader
    assert_eq!(
        state.sequence().tracks[0].gain_db.evaluate_at(TimeTick(0)).as_scalar(),
        Some(0.0)
    );
}

#[test]
fn track_pan_is_clamped_to_the_stereo_field() {
    // The mixer's pan law is defined on -1..1; a value outside it would be
    // clamped silently inside the mixer, so the stored value would disagree
    // with what you hear.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.set_track_pan(track, 5.0);
    assert_eq!(state.sequence().tracks[0].pan, 1.0);
    state.set_track_pan(track, -5.0);
    assert_eq!(state.sequence().tracks[0].pan, -1.0);
}

#[test]
fn mixer_settings_survive_a_save_load_round_trip() {
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.set_track_gain(track, -4.5, false);
    state.set_track_pan(track, 0.75);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mix.nleproj");
    state.save_to(&path);
    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&path));

    let t = &fresh.sequence().tracks[0];
    assert_eq!(t.gain_db.evaluate_at(TimeTick(0)).as_scalar(), Some(-4.5));
    assert_eq!(t.pan, 0.75);
}

#[test]
fn the_master_fader_is_session_state_not_project_state() {
    // A monitoring choice must not follow the file: opening a project saved
    // by someone who had pulled the master down should not silently export
    // quiet.
    let (mut state, _) = state_with_three_clips();
    state.master_gain_db = -20.0;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.nleproj");
    state.save_to(&path);

    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&path));
    assert_eq!(fresh.master_gain_db, 0.0, "master gain must not persist into the project");
}

#[test]
fn an_autosave_snapshot_round_trips_but_does_not_claim_the_project_path() {
    // The trap this guards: if a recovery write adopted its own path as the
    // project's location, the next Ctrl+S would silently overwrite the
    // recovery file instead of the user's project — and "Save" would stop
    // producing a file the user could find.
    let (state, _) = state_with_three_clips();
    let dir = tempfile::tempdir().unwrap();
    let recovery = dir.path().join(".recover-x.nleproj");

    state.write_snapshot_to(&recovery).expect("snapshot should write");
    assert!(recovery.exists());
    assert_eq!(state.project_path, None, "autosave must not claim the project path");
    assert!(
        !state.status.contains("saved"),
        "autosave must not overwrite the status line, found {:?}",
        state.status
    );

    // And it's a real, loadable project, not a truncated one.
    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&recovery));
    assert_eq!(fresh.sequence().tracks[0].clips.len(), 3);
}

#[test]
fn a_transition_added_in_the_editor_reaches_the_compiled_graph() {
    // Bridges the two halves that are each tested on their own: the state
    // layer writing a transition, and the compositor blending one. If the
    // preview shows no dissolve, this says which side is at fault.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.add_transition(
        track,
        TimeTick(TIMEBASE),
        timeline::TransitionKind::CrossDissolve,
        TimeTick(TIMEBASE / 2),
    );

    let compiler = render::GraphCompiler::new(render::BuiltinRegistry::default());
    // Region is 0.75s..1.25s, so the cut at 1s is the midpoint.
    let graph = compiler
        .compile(state.project(), state.seq_id, TimeTick(TIMEBASE))
        .expect("sequence compiles");
    let plan = &graph.track_plans[0];
    let tr = plan
        .transition
        .as_ref()
        .expect("the transition the editor added must appear in the graph");
    assert!((tr.progress - 0.5).abs() < 1e-4, "progress at the cut should be 0.5, got {}", tr.progress);
    assert!(tr.outgoing.is_some(), "the clip before the cut should be the outgoing layer");
    assert_eq!(graph.media_requests().len(), 2, "both layers need a frame");
}

#[test]
fn a_transition_at_a_clips_head_dissolves_up_from_nothing() {
    // The exact case exercised by hand in the editor: a dissolve on the
    // first clip's head, with the playhead at 0. The region straddles tick
    // 0, so progress there is 0.5 and the clip must be drawn half-strength.
    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.add_transition(
        track,
        TimeTick(0),
        timeline::TransitionKind::CrossDissolve,
        TimeTick(TIMEBASE),
    );

    let compiler = render::GraphCompiler::new(render::BuiltinRegistry::default());
    let graph = compiler
        .compile(state.project(), state.seq_id, state.display_tick())
        .expect("compiles");
    let tr = graph.track_plans[0]
        .transition
        .as_ref()
        .expect("a head transition should be active at tick 0");
    assert!(tr.outgoing.is_none(), "nothing precedes the sequence start");
    assert!(
        (tr.progress - 0.5).abs() < 1e-4,
        "a 1s transition centred on tick 0 is half done at tick 0, got {}",
        tr.progress
    );
}

#[test]
fn the_preview_actually_dims_a_clip_under_a_head_dissolve() {
    // Closes the last gap in the transition chain by *measuring* it. The
    // graph side and the compositor side are each tested on their own, but
    // nothing rendered a real `EditorState` and checked the resulting pixel
    // — and a saturated-red preview at 180/255 versus 255/255 is exactly
    // the difference an eye can't be trusted on in a screenshot.
    let Some((device, queue)) = render::headless_context() else {
        panic!("no GPU adapter — this project cannot run without one, so this is a failure");
    };
    let compositor = render::Compositor::new(device, queue);
    let compiler = render::GraphCompiler::new(render::BuiltinRegistry::default());

    let (mut state, _) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let asset = media::MediaAssetId(1);

    // A solid opaque red source for the clip to show.
    let (w, h) = (16u32, 16u32);
    let red: Vec<u8> = [255u8, 0, 0, 255].iter().copied().cycle().take((w * h * 4) as usize).collect();
    let colour = media::ColorMetadata {
        primaries: media::ColorPrimaries::Rec709,
        transfer: media::TransferFunction::Bt709,
        matrix: media::MatrixCoefficients::Bt709,
        full_range: true,
    };

    let render_at_playhead = |state: &EditorState| -> [u8; 4] {
        let mut sources = render::SourceFrames::default();
        sources.insert(asset, 0, compositor.upload_rgba(&red, w, h, colour));
        let graph = compiler
            .compile(state.project(), state.seq_id, state.display_tick())
            .expect("compiles");
        let (frame, _) = compositor.render_to_rgba(&graph, &sources, render::DeliverySpace::Rec709);
        frame.pixel(frame.width / 2, frame.height / 2)
    };

    // Playhead 0, no transition: the clip at full strength.
    state.playhead = 0;
    let plain = render_at_playhead(&state);
    assert!(plain[0] > 250, "baseline should be full red, got {plain:?}");

    // A 1s dissolve on the clip's head is half done at tick 0, so the clip
    // draws at 50% over nothing.
    state.add_transition(
        track,
        TimeTick(0),
        timeline::TransitionKind::CrossDissolve,
        TimeTick(TIMEBASE),
    );
    let dissolving = render_at_playhead(&state);

    let expected = (render::color::linear_to_rec709(0.5) * 255.0).round() as u8;
    assert!(
        dissolving[0].abs_diff(expected) <= 4,
        "under a half-done dissolve the clip should read ~{expected}/255, got {}",
        dissolving[0]
    );
    assert!(
        plain[0] - dissolving[0] > 40,
        "the dissolve must visibly darken the frame: {} -> {}",
        plain[0],
        dissolving[0]
    );

    // And undoing it restores the full-strength frame, which is what the
    // editor's undo has to mean for a transition.
    state.undo.undo();
    assert_eq!(render_at_playhead(&state), plain, "undo should restore the plain frame");
}

#[test]
fn a_reconstructed_history_cannot_disagree_with_its_project_even_hand_edited() {
    // Schema v3 stored `after` per entry and validated it against the
    // project, dropping the whole history on a mismatch (a hand-edited or
    // buggy-build file could otherwise send the first Ctrl+Z to an
    // unrelated state). Schema v4 removes the redundant copy instead of
    // checking it: `after` is now always derived as "the next entry's
    // `before`, or the project itself for the last one" — so there is
    // nothing left that could disagree. This proves that by construction:
    // even a hand-edited `before` still reconstructs to a chain ending
    // exactly at the saved project.
    let (mut state, ids) = state_with_three_clips();
    state.select_only(ids[2]);
    state.ripple_delete_selection();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edited.nleproj");
    state.save_to(&path);

    let mut doc = project::load(&path).unwrap();
    assert!(!doc.undo_history.is_empty(), "precondition: the file has history");
    // Hand-edit the stored `before` to something arbitrary — the kind of
    // edit that used to produce a stored `after` disagreeing with reality.
    doc.undo_history.last_mut().unwrap().before.sequences[0].tracks[0].clips.clear();
    project::save(&doc, &path).unwrap();

    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&path), "the project must still open");
    let expected = state.sequence().tracks[0].clips.len();
    assert_eq!(
        fresh.sequence().tracks[0].clips.len(),
        expected,
        "the loaded project is unaffected by a hand-edited history entry"
    );
    assert!(fresh.undo.undo(), "undo must still be offered — there is no disagreement to detect");
    assert!(!fresh.status.contains("discard"), "there is no longer a discard outcome to report");
}

#[test]
fn saved_history_is_capped_to_the_most_recent_entries() {
    // The other half of the size fix: even an in-memory stack at its full
    // 100-entry cap must not write more than
    // `project::MAX_PERSISTED_UNDO_ENTRIES` of them to disk.
    let (mut state, _ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    for i in 0..(command::UndoStack::DEFAULT_MAX_HISTORY + 10) {
        // A track-gain change is a real, distinct project version and
        // always pushes — unlike marks or selection, which are session
        // state and never touch the undo stack.
        state.set_track_gain(track, (i % 5) as f64, false);
    }
    assert_eq!(
        state.undo.history().len(),
        command::UndoStack::DEFAULT_MAX_HISTORY,
        "precondition: the in-memory stack is at its cap"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("many.nleproj");
    state.save_to(&path);

    let doc = project::load(&path).unwrap();
    assert_eq!(
        doc.undo_history.len(),
        project::MAX_PERSISTED_UNDO_ENTRIES,
        "the file must carry only the persisted cap, not the full in-memory history"
    );

    // And what's kept is still a correct, undoable chain ending at the
    // saved project — the most recent entries, not an arbitrary slice.
    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&path));
    assert!(fresh.undo.undo(), "the kept history must still be usable");
}

#[test]
fn undo_still_works_after_a_save_and_reopen() {
    // The whole promise, and what was missing: `undo_history` was declared
    // in schema v1 and written as an empty list every single time, so
    // reopening a project silently threw away the ability to undo the work
    // in it. Everything else here is bookkeeping around this assertion.
    let (mut state, ids) = state_with_three_clips();
    state.select_only(ids[1]);
    state.ripple_delete_selection();
    assert_eq!(state.sequence().tracks[0].clips.len(), 2);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.nleproj");
    state.save_to(&path);

    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&path));
    assert_eq!(fresh.sequence().tracks[0].clips.len(), 2, "loads the saved state");

    assert!(fresh.undo.undo(), "there should be history to undo into");
    assert_eq!(
        fresh.sequence().tracks[0].clips.len(),
        3,
        "undo after reopen must bring the deleted clip back"
    );
    assert_eq!(clip_starts(&fresh), vec![0, TIMEBASE, TIMEBASE * 2]);
}

#[test]
fn a_reopened_project_can_redo_what_it_just_undid() {
    // Redo isn't *persisted*, but undoing after a reopen has to create a
    // redo entry like any other undo — otherwise restored history is a
    // one-way trip.
    let (mut state, ids) = state_with_three_clips();
    state.select_only(ids[0]);
    state.ripple_delete_selection();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.nleproj");
    state.save_to(&path);

    let mut fresh = EditorState::new();
    fresh.open_from(&path);
    assert!(!fresh.undo.redo(), "nothing to redo before undoing");
    fresh.undo.undo();
    assert!(fresh.undo.redo(), "undo after reopen must be redoable");
    assert_eq!(fresh.sequence().tracks[0].clips.len(), 2);
}

#[test]
fn an_autosave_snapshot_carries_no_history_so_its_cost_stays_flat() {
    // Autosave runs every 20 seconds; if it serialised the undo stack its
    // cost would grow with session length, which is the opposite of what a
    // safety net should do.
    let (mut state, ids) = state_with_three_clips();
    state.select_only(ids[0]);
    state.ripple_delete_selection();
    assert!(!state.undo.history().is_empty());

    let dir = tempfile::tempdir().unwrap();
    let recovery = dir.path().join(".recover-x.nleproj");
    state.write_snapshot_to(&recovery).unwrap();

    let doc = project::load(&recovery).unwrap();
    assert!(doc.undo_history.is_empty(), "recovery writes must skip history");
    // But the work itself is still there — that's the part that matters.
    assert_eq!(doc.project.sequences[0].tracks[0].clips.len(), 2);
}

#[test]
fn a_project_saved_without_history_still_opens() {
    // Every project written before this change has `undo_history: []`.
    let (state, _) = state_with_three_clips();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nohist.nleproj");
    let doc = project::ProjectDocument::new((**state.project()).clone(), vec![]);
    assert!(doc.undo_history.is_empty());
    project::save(&doc, &path).unwrap();

    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&path));
    assert!(!fresh.undo.undo(), "no history to undo, and that's fine");
    assert_eq!(fresh.sequence().tracks[0].clips.len(), 3);
}

#[test]
fn persisted_undo_history_size_is_measured_not_assumed() {
    // `docs/decisions-log.md` bounds history "to keep autosave file size
    // predictable" — but nobody ever measured what it actually costs, and
    // each `PersistedCommand` stores TWO full project snapshots. This puts
    // a number on it and fails if that number becomes unreasonable.
    let mut state = EditorState::new();
    let mut project = (**state.project()).clone();
    let mut clips = Vec::new();
    for i in 0..40i64 {
        let start = i * TIMEBASE;
        let mut c = ClipInstance {
            id: ClipInstanceId(1000 + i as u64),
            source: ClipSource::Media(media::MediaAssetId(1)),
            source_in: TimeTick(0),
            source_out: TimeTick(TIMEBASE),
            timeline_in: TimeTick(start),
            timeline_out: TimeTick(start + TIMEBASE),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        };
        // A keyframed effect, so clips aren't unrealistically tiny.
        let mut params = std::collections::BTreeMap::new();
        let mut track = timeline::ParamTrack::constant(ParamValue::Number(1.0));
        for k in 0..8 {
            track.upsert_keyframe(
                TimeTick(k * TIMEBASE / 8),
                ParamValue::Number(k as f64),
                timeline::InterpolationMode::Linear,
            );
        }
        params.insert("sigma".to_string(), track);
        c.effects.push(timeline::EffectInstance {
            id: timeline::EffectInstanceId(2000 + i as u64),
            effect_type: "gaussian_blur".into(),
            enabled: true,
            params,
        });
        clips.push(c);
    }
    project.sequences[0].tracks.push(Track {
        id: TrackId(1),
        kind: TrackKind::Video,
        name: "V1".into(),
        clips,
        transitions: vec![],
        gain_db: timeline::unity_gain(),
        pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    });
    project.assets.push(fake_asset(1, "C:/media/clip.mp4"));
    state.undo.push("setup", std::sync::Arc::new(project));

    let dir = tempfile::tempdir().unwrap();
    let no_history = dir.path().join("plain.nleproj");
    state.write_snapshot_to(&no_history).unwrap();
    let baseline = std::fs::metadata(&no_history).unwrap().len();

    // Fill the bounded history right up to its cap.
    let cap = command::UndoStack::DEFAULT_MAX_HISTORY;
    for i in 0..cap {
        let mut p = (**state.project()).clone();
        p.sequences[0].tracks[0].clips[i % 40].timeline_in = TimeTick(i as i64);
        state.undo.push("edit", std::sync::Arc::new(p));
    }
    assert_eq!(state.undo.history().len(), cap, "history should be at its cap");

    let with_history = dir.path().join("full.nleproj");
    state.save_to(&with_history);
    let full = std::fs::metadata(&with_history).unwrap().len();

    let multiplier = full as f64 / baseline as f64;
    println!(
        "project {:.1} KB alone, {:.1} KB with {cap} undo entries ({multiplier:.0}x)",
        baseline as f64 / 1024.0,
        full as f64 / 1024.0
    );

    // Two things make the naive encoding wasteful, and both are fixed:
    //  - every entry stored `before` AND `after`, but `history[i].after` is
    //    always `history[i+1].before`, so half the bytes were duplicates;
    //  - all 100 in-memory entries were written, when far fewer are useful
    //    after reopening a project.
    // Measured at 199x the bare project before those fixes.
    assert!(
        multiplier < 40.0,
        "undo history inflated the project file {multiplier:.0}x ({:.1} MB) —              the before/after duplication or the persisted cap has regressed",
        full as f64 / (1024.0 * 1024.0)
    );
}

#[test]
fn parking_at_the_sequence_end_still_renders_the_last_frame() {
    // Found by watching real playback: it stops with the playhead exactly on
    // the duration, which is an exclusive out point, so the graph compiled
    // there had no active clip and the preview went black the moment
    // playback finished.
    let (mut state, _) = state_with_three_clips();
    let end = state.sequence_duration_ticks();
    assert_eq!(end, TIMEBASE * 3);

    state.playhead = end;
    assert_eq!(
        state.display_tick(),
        TimeTick(end - 1),
        "at the out point the preview must fall back inside the last frame"
    );

    // Everywhere else it's the playhead untouched.
    state.playhead = TIMEBASE;
    assert_eq!(state.display_tick(), TimeTick(TIMEBASE));
    state.playhead = -50;
    assert_eq!(state.display_tick(), TimeTick(0), "never negative");
}

#[test]
fn display_tick_is_zero_for_an_empty_sequence() {
    // An empty sequence has duration 0, so the clamp's "one tick inside"
    // would go negative.
    let state = EditorState::new();
    assert_eq!(state.sequence_duration_ticks(), 0);
    assert_eq!(state.display_tick(), TimeTick(0));
}

#[test]
fn lift_leaves_a_gap_and_ripple_delete_closes_it() {
    // The distinction that makes both shortcuts worth having, and the one
    // that's silently wrong if Delete and Shift+Delete are wired to the
    // same op.
    let (mut state, ids) = state_with_three_clips();
    state.select_only(ids[1]); // the middle clip
    state.lift_selection();
    assert_eq!(
        clip_starts(&state),
        vec![0, TIMEBASE * 2],
        "lift must leave the third clip where it was"
    );

    let (mut state, ids) = state_with_three_clips();
    state.select_only(ids[1]);
    state.ripple_delete_selection();
    assert_eq!(
        clip_starts(&state),
        vec![0, TIMEBASE],
        "ripple delete must pull the third clip back to close the gap"
    );
}

#[test]
fn deleting_a_multi_clip_selection_is_a_single_undo_step() {
    // One Ctrl+Z should restore everything the delete removed. Pushing one
    // undo entry per clip would make the user press undo N times and see
    // half-deleted intermediate states.
    let (mut state, ids) = state_with_three_clips();
    state.selected_clips = vec![ids[0], ids[1]];
    state.ripple_delete_selection();
    assert_eq!(state.sequence().tracks[0].clips.len(), 1);

    state.undo.undo();
    assert_eq!(
        state.sequence().tracks[0].clips.len(),
        3,
        "a single undo must bring back both deleted clips"
    );
}

#[test]
fn a_failed_multi_op_leaves_the_project_untouched() {
    // All-or-nothing: a selection containing something undeletable must not
    // half-apply, or the user is left with a project neither state.
    let (mut state, ids) = state_with_three_clips();
    let before = state.project().clone();
    let ops = vec![
        EditOp::Extract { clip: ids[0] },
        EditOp::Extract { clip: ClipInstanceId(99999) }, // does not exist
    ];
    assert!(!state.apply_ops("delete", &ops));
    assert!(
        std::sync::Arc::ptr_eq(state.project(), &before),
        "a failed batch must not push a new project version at all"
    );
}

#[test]
fn paste_gives_the_copy_a_fresh_id_and_keeps_relative_spacing() {
    // Reusing the source ID would put two clips with the same identity in
    // one sequence, and every op locates clips by ID — the duplicate would
    // shadow the original and edits would hit the wrong one.
    let (mut state, ids) = state_with_three_clips();
    state.selected_clips = vec![ids[0], ids[1]];
    state.copy_selection();
    assert_eq!(state.clipboard.len(), 2);

    state.playhead = TIMEBASE * 10;
    state.paste_at_playhead();

    let clips = &state.sequence().tracks[0].clips;
    let pasted: Vec<&ClipInstance> =
        clips.iter().filter(|c| c.timeline_in.0 >= TIMEBASE * 10).collect();
    assert_eq!(pasted.len(), 2, "both copied clips should land");
    let mut starts: Vec<i64> = pasted.iter().map(|c| c.timeline_in.0).collect();
    starts.sort_unstable();
    assert_eq!(
        starts,
        vec![TIMEBASE * 10, TIMEBASE * 11],
        "the 1s gap between the originals must be preserved"
    );
    for p in &pasted {
        assert!(!ids.contains(&p.id), "pasted clip reused a source ID: {:?}", p.id);
    }
    // And no duplicate IDs anywhere in the sequence.
    let mut all: Vec<u64> = clips.iter().map(|c| c.id.0).collect();
    all.sort_unstable();
    let count = all.len();
    all.dedup();
    assert_eq!(all.len(), count, "sequence has duplicate clip IDs after paste");
}

#[test]
fn paste_survives_deleting_the_clip_it_was_copied_from() {
    // The clipboard stores whole clips, not references — copy, delete,
    // paste is an ordinary move-by-cut-and-paste and must work.
    let (mut state, ids) = state_with_three_clips();
    state.select_only(ids[0]);
    state.copy_selection();
    state.ripple_delete_selection();
    state.playhead = TIMEBASE * 5;
    state.paste_at_playhead();
    assert!(
        state
            .sequence()
            .tracks[0]
            .clips
            .iter()
            .any(|c| c.timeline_in.0 == TIMEBASE * 5),
        "paste should still work after the source clip is gone"
    );
}

#[test]
fn snapping_takes_the_nearest_candidate_and_ignores_distant_ones() {
    let (mut state, _) = state_with_three_clips();
    state.playhead = 0;
    let tol = TIMEBASE / 10;

    // Just shy of the 1s clip boundary — should snap onto it.
    assert_eq!(state.snap_tick(TIMEBASE - 1000, None, tol), TIMEBASE);
    // Far from everything — must be left exactly alone, or dragging becomes
    // impossible anywhere near a busy timeline.
    let free = TIMEBASE / 2;
    assert_eq!(state.snap_tick(free, None, tol), free);
}

#[test]
fn a_dragged_clip_cannot_snap_to_its_own_edges() {
    // Without the exclusion, a clip snaps to where it already is and can
    // never be nudged off that spot.
    let (mut state, ids) = state_with_three_clips();
    state.playhead = -1; // keep the playhead out of the candidate set
    let tol = TIMEBASE / 10;
    // Clip 3's *end* at 3s, which is the only edge in this layout belonging
    // to exactly one clip. Its start at 2s is also clip 2's end, so
    // excluding clip 3 wouldn't remove that candidate — abutting clips
    // share a boundary, and snapping there is correct, not a bug.
    let near_own_end = TIMEBASE * 3 + 1000;

    assert_eq!(
        state.snap_tick(near_own_end, None, tol),
        TIMEBASE * 3,
        "without exclusion it snaps to the clip's own end"
    );
    assert_eq!(
        state.snap_tick(near_own_end, Some(ids[2]), tol),
        near_own_end,
        "excluding the dragged clip must leave the tick alone"
    );
}

#[test]
fn snapping_disabled_is_a_no_op() {
    let (mut state, _) = state_with_three_clips();
    state.snapping = false;
    let t = TIMEBASE - 1000;
    assert_eq!(state.snap_tick(t, None, TIMEBASE / 10), t);
}

#[test]
fn marks_never_end_up_inverted() {
    // An out point before the in point would make every range operation
    // need its own "is this backwards" check.
    let mut state = EditorState::new();
    state.playhead = TIMEBASE * 5;
    state.mark_out();
    state.playhead = TIMEBASE * 8;
    state.mark_in(); // in after out
    assert_eq!(state.in_point, Some(TIMEBASE * 8));
    assert_eq!(state.out_point, None, "the now-invalid out point must be dropped");
    assert!(state.marked_range().is_none());

    state.playhead = TIMEBASE * 9;
    state.mark_out();
    assert_eq!(state.marked_range(), Some((TIMEBASE * 8, TIMEBASE * 9)));
}

#[test]
fn ctrl_click_toggles_membership_and_re_adding_promotes_to_primary() {
    // The effects panel edits the primary selection, so a Ctrl+click that
    // brings a clip *into* the selection must also bring it into focus.
    // Ctrl+clicking one that's already selected removes it — that's the
    // point of a toggle, not a bug to work around.
    let (mut state, ids) = state_with_three_clips();
    state.toggle_in_selection(ids[0]);
    state.toggle_in_selection(ids[1]);
    assert_eq!(state.selected_clips, vec![ids[0], ids[1]]);
    assert_eq!(state.primary_selection(), Some(ids[1]));

    state.toggle_in_selection(ids[0]); // deselect
    assert_eq!(state.selected_clips, vec![ids[1]]);

    state.toggle_in_selection(ids[0]); // re-select, now last = primary
    assert_eq!(state.selected_clips, vec![ids[1], ids[0]]);
    assert_eq!(
        state.primary_selection(),
        Some(ids[0]),
        "a clip brought back into the selection must become the primary one"
    );
}

/// A clip at 1s..2s on the timeline carrying one keyframeable param, so
/// the clip-relative conversion is actually exercised (a clip starting at
/// 0 would make local and sequence time identical and hide the bug).
fn state_with_keyframeable_clip() -> (EditorState, ClipInstanceId, timeline::EffectInstanceId) {
    let mut state = state_with_high_ids();
    let clip_id = ClipInstanceId(900);
    let effect_id = timeline::EffectInstanceId(1234);
    let mut project = (**state.project()).clone();
    let clip = &mut project.sequences[0].tracks[0].clips[0];
    clip.timeline_in = TimeTick(TIMEBASE);
    clip.timeline_out = TimeTick(TIMEBASE * 2);
    clip.effects[0].params.insert(
        "sigma".to_string(),
        timeline::ParamTrack::constant(ParamValue::Number(1.0)),
    );
    state.undo.push("setup kf", std::sync::Arc::new(project));
    (state, clip_id, effect_id)
}

fn track_of(
    state: &EditorState,
    clip_id: ClipInstanceId,
    effect_id: timeline::EffectInstanceId,
) -> timeline::ParamTrack {
    let (_, clip) = state.find_clip(clip_id).unwrap();
    clip.effects
        .iter()
        .find(|e| e.id == effect_id)
        .unwrap()
        .params
        .get("sigma")
        .unwrap()
        .clone()
}

#[test]
fn keyframe_times_are_clip_relative_not_sequence_relative() {
    // The bug this guards: writing the sequence tick as the keyframe time.
    // `render::graph` evaluates params at `playhead - clip.timeline_in`, so
    // a keyframe stored at sequence time would animate at the wrong moment
    // — and would break entirely as soon as the clip was moved.
    let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
    // Playhead 1.5s: half a second into a clip that starts at 1s.
    state.playhead = TIMEBASE + TIMEBASE / 2;
    let local = state.playhead_local_to_clip(clip_id).unwrap();
    assert_eq!(local, TimeTick(TIMEBASE / 2), "local time must subtract timeline_in");

    state.toggle_param_animation(clip_id, effect_id, "sigma", local);
    let track = track_of(&state, clip_id, effect_id);
    assert_eq!(track.keyframes.len(), 1);
    assert_eq!(
        track.keyframes[0].at,
        TimeTick(TIMEBASE / 2),
        "keyframe must be stored at clip-relative time"
    );
}

#[test]
fn playhead_outside_the_clip_has_no_local_time() {
    let (mut state, clip_id, _) = state_with_keyframeable_clip();
    state.playhead = 0; // before the clip, which starts at 1s
    assert!(state.playhead_local_to_clip(clip_id).is_none());
    state.playhead = TIMEBASE * 3; // after it ends at 2s
    assert!(state.playhead_local_to_clip(clip_id).is_none());
    // The out point is exclusive — the frame at timeline_out belongs to
    // whatever comes next, so keyframing there would target the wrong clip.
    state.playhead = TIMEBASE * 2;
    assert!(state.playhead_local_to_clip(clip_id).is_none());
}

#[test]
fn editing_an_animated_param_writes_a_keyframe_instead_of_the_constant() {
    // Without this, the slider would edit `default` — which `evaluate_at`
    // ignores completely once any keyframe exists — so dragging it would
    // appear to do nothing at all.
    let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
    state.playhead = TIMEBASE; // clip start, local 0
    let local = state.playhead_local_to_clip(clip_id).unwrap();
    state.toggle_param_animation(clip_id, effect_id, "sigma", local);

    // Move to 0.5s into the clip and set a different value.
    state.playhead = TIMEBASE + TIMEBASE / 2;
    let later = state.playhead_local_to_clip(clip_id).unwrap();
    state.set_param_value_at(
        clip_id,
        effect_id,
        "sigma",
        ParamValue::Number(9.0),
        Some(later),
        false,
    );

    let track = track_of(&state, clip_id, effect_id);
    assert_eq!(track.keyframes.len(), 2, "should have added a second keyframe");
    assert_eq!(
        track.evaluate_at(later).as_scalar(),
        Some(9.0),
        "the new value must be what the renderer reads at that tick"
    );
    assert_eq!(
        track.evaluate_at(TimeTick(0)).as_scalar(),
        Some(1.0),
        "the original keyframe must keep its value — this is an animation, not a constant"
    );
}

#[test]
fn a_non_animated_param_still_edits_its_constant() {
    let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
    state.playhead = TIMEBASE;
    let local = state.playhead_local_to_clip(clip_id);
    state.set_param_value_at(
        clip_id,
        effect_id,
        "sigma",
        ParamValue::Number(5.0),
        local,
        false,
    );
    let track = track_of(&state, clip_id, effect_id);
    assert!(!track.is_animated(), "editing a constant must not create keyframes");
    assert_eq!(track.default.as_scalar(), Some(5.0));
}

#[test]
fn toggling_animation_off_holds_the_value_at_the_playhead() {
    // Premiere's behaviour, and the non-surprising one: turning the
    // stopwatch off must not change the frame you're looking at. Collapsing
    // to the old `default` instead would make the picture jump.
    let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
    state.playhead = TIMEBASE;
    let start = state.playhead_local_to_clip(clip_id).unwrap();
    state.toggle_param_animation(clip_id, effect_id, "sigma", start);
    state.playhead = TIMEBASE + TIMEBASE / 2;
    let mid = state.playhead_local_to_clip(clip_id).unwrap();
    state.set_param_value_at(clip_id, effect_id, "sigma", ParamValue::Number(8.0), Some(mid), false);

    // Park a quarter into the clip, where the curve reads ~4.5 (halfway
    // between 1.0 at local 0 and 8.0 at local 0.5s), then de-animate.
    state.playhead = TIMEBASE + TIMEBASE / 4;
    let quarter = state.playhead_local_to_clip(clip_id).unwrap();
    let before = track_of(&state, clip_id, effect_id)
        .evaluate_at(quarter)
        .as_scalar()
        .unwrap();
    state.toggle_param_animation(clip_id, effect_id, "sigma", quarter);

    let track = track_of(&state, clip_id, effect_id);
    assert!(!track.is_animated());
    let after = track.evaluate_at(quarter).as_scalar().unwrap();
    assert!(
        (after - before).abs() < 1e-9,
        "de-animating changed the rendered value from {before} to {after}"
    );
    assert!(
        (before - 4.5).abs() < 0.01,
        "sanity: the linear curve should read ~4.5 a quarter in, got {before}"
    );
}

#[test]
fn set_keyframe_tangents_writes_the_handles_at_the_playhead_and_is_undoable() {
    // The engine has evaluated `Keyframe::tangents` since M4a and the file
    // format has always stored them, but nothing could write one — this is
    // the state-layer half of the tangent-handle drag UI.
    let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
    state.playhead = TIMEBASE;
    let local = state.playhead_local_to_clip(clip_id).unwrap();
    state.toggle_param_animation(clip_id, effect_id, "sigma", local);
    state.set_keyframe_interpolation(
        clip_id,
        effect_id,
        "sigma",
        local,
        timeline::InterpolationMode::Bezier,
    );

    let tangents = ((-30.0, -0.4), (30.0, 0.4));
    state.set_keyframe_tangents(clip_id, effect_id, "sigma", local, tangents);

    let track = track_of(&state, clip_id, effect_id);
    assert_eq!(track.keyframes[0].tangents, Some(tangents));

    state.undo.undo();
    assert_eq!(
        track_of(&state, clip_id, effect_id).keyframes[0].tangents,
        None,
        "undo should remove the tangent edit like any other keyframe change"
    );
}

#[test]
fn animating_an_off_clip_param_leaves_the_curve_alone() {
    // With the playhead off the clip there's no keyframe time to write to.
    // Falling back to editing `default` would silently corrupt the
    // animation with a value the renderer never shows.
    let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
    state.playhead = TIMEBASE;
    let local = state.playhead_local_to_clip(clip_id).unwrap();
    state.toggle_param_animation(clip_id, effect_id, "sigma", local);
    let before = track_of(&state, clip_id, effect_id);

    state.playhead = 0; // off the clip
    state.set_param_value_at(clip_id, effect_id, "sigma", ParamValue::Number(99.0), None, false);

    assert_eq!(track_of(&state, clip_id, effect_id), before, "curve must be untouched");
}

#[test]
fn keyframe_edits_are_undoable() {
    let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
    state.playhead = TIMEBASE;
    let local = state.playhead_local_to_clip(clip_id).unwrap();
    state.toggle_param_animation(clip_id, effect_id, "sigma", local);
    assert!(track_of(&state, clip_id, effect_id).is_animated());
    state.undo.undo();
    assert!(
        !track_of(&state, clip_id, effect_id).is_animated(),
        "undo must remove the keyframe the stopwatch added"
    );
}

#[test]
fn open_resolves_media_that_moved_with_the_project_folder() {
    // The whole point of storing a relative path: copy the project
    // folder somewhere else and the media still resolves, even though
    // every recorded absolute path is now wrong.
    let original = tempfile::tempdir().unwrap();
    let media = original.path().join("footage").join("a.mp4");
    std::fs::create_dir_all(media.parent().unwrap()).unwrap();
    std::fs::write(&media, b"x").unwrap();

    let mut state = EditorState::new();
    let mut project = (**state.project()).clone();
    project.assets.push(fake_asset(1, &media.to_string_lossy()));
    state.undo.push("setup", std::sync::Arc::new(project));
    let proj_path = original.path().join("p.nleproj");
    state.save_to(&proj_path);

    // Simulate the move: same layout, different parent directory.
    let moved = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(moved.path().join("footage")).unwrap();
    std::fs::copy(&media, moved.path().join("footage").join("a.mp4")).unwrap();
    let moved_proj = moved.path().join("p.nleproj");
    std::fs::copy(&proj_path, &moved_proj).unwrap();

    let mut fresh = EditorState::new();
    assert!(fresh.open_from(&moved_proj));
    assert_eq!(
        fresh.asset_paths.get(&media::MediaAssetId(1)),
        Some(&moved.path().join("footage").join("a.mp4")),
        "should resolve via the relative path, not the stale absolute one"
    );
}

#[test]
fn a_project_file_cannot_point_its_relative_media_path_anywhere_on_disk() {
    // `relative_path` is joined onto the project's own folder, and
    // `Path::join` silently throws the folder away when handed an absolute
    // path — so the field marked "relative" could name any file on the
    // machine, and the editor would open and try to decode it. A project
    // file arrives by download or email like any other document.
    let base = std::path::Path::new("C:/projects/mine");
    assert_eq!(
        super::project_io::resolve_relative_media(base, "footage/a.mp4"),
        Some(base.join("footage/a.mp4")),
        "an ordinary relative path must still resolve"
    );
    for hostile in ["C:/Windows/System32/config/SAM", "/etc/shadow", "../../../secrets.mp4", r"..\..\secrets.mp4"] {
        assert_eq!(
            super::project_io::resolve_relative_media(base, hostile),
            None,
            "{hostile} must not resolve"
        );
    }
}

#[test]
fn a_keystroke_during_a_drag_commits_the_drag_instead_of_crashing() {
    // Reachable by holding the mouse down on a clip and pressing Delete.
    // The per-frame stale-drag guard deliberately doesn't fire here — the
    // pointer really is still down, the gesture really is still live — so
    // this is the one window where an edit can arrive on top of an open
    // group. It used to assert inside UndoStack::push and take the whole
    // editor, and the unsaved project, with it.
    let (mut state, ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    state.begin_drag_edit("move clip");
    state.move_clip(ids[0], track, TIMEBASE * 10);

    state.selected_clips = vec![ids[1]];
    state.lift_selection();

    // Both survive, as two separate steps: the drag is one, the delete is
    // the next, which is also the order Ctrl+Z should walk back through.
    assert!(state.find_clip(ids[1]).is_none(), "the delete must actually have happened");
    assert_eq!(
        state.find_clip(ids[0]).map(|(_, c)| c.timeline_in.0),
        Some(TIMEBASE * 10),
        "and the drag must have been committed, not discarded"
    );
    assert!(state.undo.undo(), "undoing takes back the delete");
    assert!(state.find_clip(ids[1]).is_some(), "the deleted clip comes back");
    assert!(state.undo.undo(), "undoing again takes back the drag");
    assert_eq!(
        state.find_clip(ids[0]).map(|(_, c)| c.timeline_in.0),
        Some(0),
        "the whole drag is one step, not one per mouse-move frame"
    );
}

#[test]
fn a_background_analysis_landing_mid_drag_does_not_crash() {
    // Needs no keystroke at all: poll_analysis runs every frame whatever
    // the pointer is doing, so any scene-cut or silence job finishing
    // while a clip is being dragged used to land on an open group.
    let (mut state, ids) = state_with_three_clips();
    let track = state.sequence().tracks[0].id;
    let placement = placement_now(&state, ids[2]);
    state.begin_drag_edit("move clip");
    state.move_clip(ids[0], track, TIMEBASE * 10);

    finish_analysis(
        &mut state,
        ids[2],
        AnalysisKind::SceneCuts,
        placement,
        Ok(analysis_jobs::Outcome::SceneCuts(vec![TIMEBASE / 2])),
    );
    state.poll_analysis();

    assert_eq!(
        state.find_clip(ids[0]).map(|(_, c)| c.timeline_in.0),
        Some(TIMEBASE * 10),
        "the drag must survive a background result landing on top of it"
    );
}

#[test]
fn ending_a_gesture_twice_is_harmless() {
    // end_drag_edit is called from whichever widget owns the gesture, and
    // that widget has no way to know the stack already committed the group
    // underneath it.
    let (mut state, _) = state_with_three_clips();
    state.begin_drag_edit("move clip");
    state.end_drag_edit();
    state.end_drag_edit();
    assert!(!state.coalescing_open());
}
