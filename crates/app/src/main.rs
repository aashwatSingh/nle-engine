//! M0 placeholder entry point. Not the UI (that's M3+) — this exists only to
//! prove the workspace's crates actually link together and the core types
//! behave, per the M0 "does the toolchain work at all" risk item.

use std::sync::Arc;
use timeline::{FrameRate, Project, TimeTick, Timecode};

fn main() {
    let project = Arc::new(Project { sequences: vec![], assets: vec![] });
    let mut undo = command::UndoStack::new(project.clone(), command::UndoStack::DEFAULT_MAX_HISTORY);

    let tick = TimeTick::from_frame(1500, FrameRate::Fps29_97);
    let tc = Timecode::from_tick(tick, FrameRate::Fps29_97, true);

    println!("nle-engine M0 scaffold");
    println!("1500 frames @ 29.97 drop-frame = {tc}");
    println!("undo stack starts at {} sequences", undo.current().sequences.len());

    let mut edited = (*project).clone();
    edited.sequences.push(timeline::Sequence {
        id: timeline::SequenceId(1),
        name: "Sequence 01".into(),
        settings: timeline::SequenceSettings {
            frame_rate: FrameRate::Fps29_97,
            width: 1920,
            height: 1080,
            sample_rate: 48_000,
            working_color_primaries: media::ColorPrimaries::Rec709,
            drop_frame_timecode: true,
        },
        tracks: vec![],
        markers: vec![],
    });
    undo.push("create sequence", Arc::new(edited));
    println!("after edit: {} sequence(s)", undo.current().sequences.len());
    undo.undo();
    println!("after undo: {} sequence(s)", undo.current().sequences.len());
}
