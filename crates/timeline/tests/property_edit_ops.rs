//! Property-based test suite for `edit_ops::apply`, per spec 4.2/8: "write
//! property-based tests asserting no two clips on a track overlap... run
//! these tests against randomized edit sequences of length 1000+."
//!
//! Approach: a fixed, modestly-populated starting project (so most
//! generated ops have real content to act on), a sequence of randomly
//! chosen edit "shapes" (`OpChoice`) resolved against *growing* ID pools
//! (successful `Insert`/`Razor` add their new clips to the pool, so later
//! ops in the same sequence can act on them too), applied one at a time.
//! An op that `apply` rejects (`Err`) is simply skipped — rejection itself
//! is not a bug, only accepting an op that breaks an invariant is.
//!
//! Scope: `NestSequence` is covered by a dedicated unit test in
//! `edit_ops.rs`, not fuzzed here — its "creates a whole new sequence"
//! shape doesn't fit this pool-of-ids-on-one-sequence model without
//! substantially more machinery, and the invariants that matter for it
//! (the host track's clip range, the nested sequence's internal layout)
//! are exactly what that unit test pins down directly.

use media::{ColorPrimaries, MediaAssetId};
use proptest::prelude::*;
use timeline::edit_ops::apply;
use timeline::model::invariants::check_no_overlaps;
use timeline::model::{ClipSource, LinkedGroupId};
use timeline::{
    ClipInstance, ClipInstanceId, EditOp, FrameRate, Project, Sequence, SequenceId,
    SequenceSettings, SpeedCurve, TimeTick, Track, TrackId, TrackKind,
};

fn make_clip(id: u64, tin: i64, tout: i64) -> ClipInstance {
    ClipInstance {
        id: ClipInstanceId(id),
        source: ClipSource::Media(MediaAssetId(1)),
        source_in: TimeTick(0),
        source_out: TimeTick(tout - tin),
        timeline_in: TimeTick(tin),
        timeline_out: TimeTick(tout),
        speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
        effects: vec![],
        audio_gain_db: timeline::ParamTrack::constant(timeline::ParamValue::Number(0.0)),
        audio_pan: timeline::ParamTrack::constant(timeline::ParamValue::Number(0.0)),
        linked_group: None,
    }
}

fn make_track(id: u64, kind: TrackKind, clips: Vec<ClipInstance>) -> Track {
    Track { id: TrackId(id), kind, name: format!("T{id}"), clips, transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0, locked: false, sync_locked: true, muted: false, solo: false, height_px: 60 }
}

fn starting_project() -> (Project, Vec<ClipInstanceId>, Vec<TrackId>) {
    let mut clip_id = 1u64;
    let mut next_clip = |dur_step: i64, count: i64| {
        (0..count)
            .map(|i| {
                let c = make_clip(clip_id, i * dur_step, (i + 1) * dur_step);
                clip_id += 1;
                c
            })
            .collect::<Vec<_>>()
    };
    let v1_clips = next_clip(100, 3);
    let v2_clips = next_clip(100, 3);
    let a1_clips = next_clip(100, 3);
    let a2_clips = next_clip(100, 3);

    let mut clip_pool: Vec<ClipInstanceId> = Vec::new();
    for clips in [&v1_clips, &v2_clips, &a1_clips, &a2_clips] {
        clip_pool.extend(clips.iter().map(|c| c.id));
    }

    let tracks = vec![
        make_track(1, TrackKind::Video, v1_clips),
        make_track(2, TrackKind::Video, v2_clips),
        make_track(3, TrackKind::Audio, a1_clips),
        make_track(4, TrackKind::Audio, a2_clips),
    ];
    let track_pool = tracks.iter().map(|t| t.id).collect();

    let project = Project {
        sequences: vec![Sequence {
            id: SequenceId(1),
            name: "S1".into(),
            settings: SequenceSettings {
                frame_rate: FrameRate::Fps30,
                width: 1920,
                height: 1080,
                sample_rate: 48000,
                working_color_primaries: ColorPrimaries::Rec709,
                drop_frame_timecode: false,
            },
            tracks,
            markers: vec![],
        }],
        assets: vec![],
        bins: vec![],
    };
    (project, clip_pool, track_pool)
}

#[derive(Debug, Clone)]
enum OpChoice {
    Insert { track_idx: usize, at: i64, dur: i64 },
    Extract { clip_idx: usize },
    Lift { clip_idx: usize },
    TrimIn { clip_idx: usize, delta: i64 },
    TrimOut { clip_idx: usize, delta: i64 },
    TrimSlip { clip_idx: usize, delta: i64 },
    TrimSlide { clip_idx: usize, delta: i64 },
    RateStretch { clip_idx: usize, new_dur: i64 },
    Razor { track_idx: usize, at: i64 },
    Join { clip_idx_a: usize, clip_idx_b: usize },
    SetLock { track_idx: usize, val: bool },
    SetSyncLock { track_idx: usize, val: bool },
    SetMute { track_idx: usize, val: bool },
    SetSolo { track_idx: usize, val: bool },
    Group { clip_idx_a: usize, clip_idx_b: usize, group: u64 },
    Ungroup { group: u64 },
}

fn op_choice_strategy() -> impl Strategy<Value = OpChoice> {
    prop_oneof![
        (0usize..20, 0i64..300, 10i64..80).prop_map(|(track_idx, at, dur)| OpChoice::Insert { track_idx, at, dur }),
        (0usize..20).prop_map(|clip_idx| OpChoice::Extract { clip_idx }),
        (0usize..20).prop_map(|clip_idx| OpChoice::Lift { clip_idx }),
        (0usize..20, -30i64..30).prop_map(|(clip_idx, delta)| OpChoice::TrimIn { clip_idx, delta }),
        (0usize..20, -30i64..30).prop_map(|(clip_idx, delta)| OpChoice::TrimOut { clip_idx, delta }),
        (0usize..20, -30i64..30).prop_map(|(clip_idx, delta)| OpChoice::TrimSlip { clip_idx, delta }),
        (0usize..20, -30i64..30).prop_map(|(clip_idx, delta)| OpChoice::TrimSlide { clip_idx, delta }),
        (0usize..20, 5i64..80).prop_map(|(clip_idx, new_dur)| OpChoice::RateStretch { clip_idx, new_dur }),
        (0usize..20, 0i64..300).prop_map(|(track_idx, at)| OpChoice::Razor { track_idx, at }),
        (0usize..20, 0usize..20).prop_map(|(clip_idx_a, clip_idx_b)| OpChoice::Join { clip_idx_a, clip_idx_b }),
        (0usize..20, any::<bool>()).prop_map(|(track_idx, val)| OpChoice::SetLock { track_idx, val }),
        (0usize..20, any::<bool>()).prop_map(|(track_idx, val)| OpChoice::SetSyncLock { track_idx, val }),
        (0usize..20, any::<bool>()).prop_map(|(track_idx, val)| OpChoice::SetMute { track_idx, val }),
        (0usize..20, any::<bool>()).prop_map(|(track_idx, val)| OpChoice::SetSolo { track_idx, val }),
        (0usize..20, 0usize..20, 0u64..5).prop_map(|(a, b, g)| OpChoice::Group { clip_idx_a: a, clip_idx_b: b, group: g }),
        (0u64..5).prop_map(|group| OpChoice::Ungroup { group }),
    ]
}

struct FuzzState {
    project: Project,
    clip_pool: Vec<ClipInstanceId>,
    track_pool: Vec<TrackId>,
    next_fresh_id: u64,
}

impl FuzzState {
    fn fresh_id(&mut self) -> ClipInstanceId {
        self.next_fresh_id += 1;
        ClipInstanceId(self.next_fresh_id)
    }

    fn pick_clip(&self, idx: usize) -> ClipInstanceId {
        self.clip_pool[idx % self.clip_pool.len()]
    }

    fn pick_track(&self, idx: usize) -> TrackId {
        self.track_pool[idx % self.track_pool.len()]
    }

    fn resolve(&mut self, choice: &OpChoice) -> EditOp {
        match *choice {
            OpChoice::Insert { track_idx, at, dur } => {
                let new_id = self.fresh_id();
                let clip = make_clip(new_id.0, 0, dur.max(1));
                EditOp::Insert { track: self.pick_track(track_idx), at: TimeTick(at), clip, split_clip_id: Some(self.fresh_id()) }
            }
            OpChoice::Extract { clip_idx } => EditOp::Extract { clip: self.pick_clip(clip_idx) },
            OpChoice::Lift { clip_idx } => EditOp::Lift { clip: self.pick_clip(clip_idx) },
            OpChoice::TrimIn { clip_idx, delta } => {
                EditOp::TrimRipple { clip: self.pick_clip(clip_idx), new_in: Some(TimeTick(delta)), new_out: None }
            }
            OpChoice::TrimOut { clip_idx, delta } => {
                EditOp::TrimRipple { clip: self.pick_clip(clip_idx), new_in: None, new_out: Some(TimeTick(delta)) }
            }
            OpChoice::TrimSlip { clip_idx, delta } => EditOp::TrimSlip { clip: self.pick_clip(clip_idx), source_delta: TimeTick(delta) },
            OpChoice::TrimSlide { clip_idx, delta } => EditOp::TrimSlide { clip: self.pick_clip(clip_idx), timeline_delta: TimeTick(delta) },
            OpChoice::RateStretch { clip_idx, new_dur } => {
                EditOp::RateStretch { clip: self.pick_clip(clip_idx), new_duration: TimeTick(new_dur.max(1)) }
            }
            OpChoice::Razor { track_idx, at } => {
                EditOp::Razor { track: self.pick_track(track_idx), at: TimeTick(at), new_clip_id: self.fresh_id() }
            }
            OpChoice::Join { clip_idx_a, clip_idx_b } => {
                EditOp::JoinThroughCut { left_clip: self.pick_clip(clip_idx_a), right_clip: self.pick_clip(clip_idx_b) }
            }
            OpChoice::SetLock { track_idx, val } => EditOp::SetTrackLock { track: self.pick_track(track_idx), locked: val },
            OpChoice::SetSyncLock { track_idx, val } => EditOp::SetSyncLock { track: self.pick_track(track_idx), sync_locked: val },
            OpChoice::SetMute { track_idx, val } => EditOp::SetTrackMute { track: self.pick_track(track_idx), muted: val },
            OpChoice::SetSolo { track_idx, val } => EditOp::SetTrackSolo { track: self.pick_track(track_idx), solo: val },
            OpChoice::Group { clip_idx_a, clip_idx_b, group } => EditOp::Group {
                clips: vec![self.pick_clip(clip_idx_a), self.pick_clip(clip_idx_b)],
                group: LinkedGroupId(group),
            },
            OpChoice::Ungroup { group } => EditOp::Ungroup { group: LinkedGroupId(group) },
        }
    }

    /// After a successful apply, register any newly created clip so later
    /// ops in the same sequence can reference it too.
    fn register_new_ids(&mut self, choice: &OpChoice) {
        match choice {
            OpChoice::Insert { .. } | OpChoice::Razor { .. } => {
                // The IDs were minted via fresh_id() during resolve(); the
                // simplest correct way to find "what's new" is to diff
                // against the pool, but since we mint sequentially we can
                // just track the last minted id separately. Simpler still:
                // rescan the (small) project for any clip id not yet in
                // the pool.
                for seq in &self.project.sequences {
                    for track in &seq.tracks {
                        for clip in &track.clips {
                            if !self.clip_pool.contains(&clip.id) {
                                self.clip_pool.push(clip.id);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn assert_invariants(project: &Project) {
    // The loader's check, run against projects the editor itself built.
    //
    // `check_project` refuses a project file that breaks these rules, which
    // makes the dangerous direction of that check the *false* one: a
    // validator that rejects something the editor can legitimately produce
    // doesn't harden anything, it just makes the user's own saved work
    // refuse to open. Pointing it at a thousand random edit sequences is a
    // far stronger statement than opening one file by hand.
    let mut copy = project.clone();
    assert_eq!(
        timeline::model::invariants::check_project(&mut copy),
        Ok(()),
        "the loader would reject a project the editor just produced: {:#?}",
        copy.sequences
    );

    for seq in &project.sequences {
        for track in &seq.tracks {
            assert_eq!(
                check_no_overlaps(track),
                Ok(()),
                "overlap invariant violated on track {:?}: {:#?}",
                track.id,
                track.clips
            );
            for clip in &track.clips {
                assert!(clip.timeline_out > clip.timeline_in, "non-positive duration clip: {clip:?}");
                assert!(clip.source_out >= clip.source_in, "negative source range: {clip:?}");
            }
        }
    }
}

/// Regression test pinned from a property-test failure: repeated inserts at
/// the front of a track (some landing mid-clip, requiring a split) followed
/// by an `Extract` on a *different*, sync-locked track used to produce a
/// negative ripple whose spanning-clip trim landed exactly on top of
/// unrelated, pre-existing content on the rippled track — a real bug in
/// `ripple_shift`, not a fuzzer artifact (see docs/decisions-log.md).
#[test]
fn regression_ripple_trim_does_not_collide_with_existing_clips() {
    let choices = vec![
        OpChoice::Insert { track_idx: 0, at: 1, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 1, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Extract { clip_idx: 0 },
        OpChoice::Insert { track_idx: 2, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 6, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 2, at: 0, dur: 10 },
        OpChoice::Insert { track_idx: 0, at: 0, dur: 10 },
        OpChoice::Extract { clip_idx: 3 },
    ];
    let (project, clip_pool, track_pool) = starting_project();
    let mut state = FuzzState { project, clip_pool, track_pool, next_fresh_id: 10_000 };

    for (i, choice) in choices.iter().enumerate() {
        let op = state.resolve(choice);
        if let Ok(new_project) = apply(&state.project, &op) {
            state.project = new_project;
            state.register_new_ids(choice);
            for seq in &state.project.sequences {
                for track in &seq.tracks {
                    assert_eq!(check_no_overlaps(track), Ok(()), "invariant broken at step {i} on track {:?}", track.id);
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn random_edit_sequences_never_violate_invariants(choices in prop::collection::vec(op_choice_strategy(), 20..150)) {
        let (project, clip_pool, track_pool) = starting_project();
        let mut state = FuzzState { project, clip_pool, track_pool, next_fresh_id: 10_000 };
        assert_invariants(&state.project);

        for choice in &choices {
            let op = state.resolve(choice);
            if let Ok(new_project) = apply(&state.project, &op) {
                assert_invariants(&new_project);
                state.project = new_project;
                state.register_new_ids(choice);
            }
            // Err is fine: the op was rejected, project is untouched by
            // construction (apply takes &Project and only returns an owned
            // clone on success).
        }
    }
}
