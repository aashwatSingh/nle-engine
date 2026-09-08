//! Transcript view for the selected media clip: local Whisper transcription
//! shown as clickable words — click one to seek there, shift-click a second
//! to select the run between them, then delete the selection to ripple-cut
//! that speech out of the timeline. This is what "transcript-based editing"
//! means in practice: editing the words edits the video.
//!
//! Deliberately does not attempt the fuller version some tools offer (typing
//! *replacement* text that re-times the video, reordering by dragging text).
//! Select-and-delete is the one operation that maps unambiguously onto this
//! project's edit model (`EditOp::Razor`+`Extract`, exactly what
//! `EditorState::delete_word_range` already does) without inventing a new
//! kind of edit; the rest is real, separate follow-up work.

use crate::state::EditorState;
use timeline::{ClipInstanceId, ClipSource};

#[derive(Default)]
pub struct TranscriptPanelState {
    /// The word index a click or drag-select started from. `None` means
    /// nothing selected. Combined with `end` to describe a range — order
    /// doesn't matter between them, `show` sorts on each use.
    anchor: Option<usize>,
    end: Option<usize>,
    /// Which clip `anchor`/`end` are indices into. Word indices only mean
    /// something relative to one clip's own transcript — without tracking
    /// this, selecting a range here then clicking a *different* clip would
    /// silently keep the stale indices, and "Delete" would act on the new
    /// clip using the old clip's selection.
    selected_clip: Option<ClipInstanceId>,
}


/// Clears the word selection whenever the panel is about to show a
/// different clip than the one the selection was made against. Kept free
/// of `egui::Ui` so it's testable without a GUI harness.
fn sync_selected_clip(panel: &mut TranscriptPanelState, clip_id: ClipInstanceId) {
    if panel.selected_clip != Some(clip_id) {
        panel.anchor = None;
        panel.end = None;
        panel.selected_clip = Some(clip_id);
    }
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState, panel: &mut TranscriptPanelState) {
    let Some(clip_id) = state.primary_selection() else {
        ui.label("Select a clip to see its transcript.");
        return;
    };
    let Some((track_id, clip)) = state.find_clip(clip_id) else {
        return;
    };
    if !matches!(clip.source, ClipSource::Media(_)) {
        // Titles, and anything else with no audio to transcribe, simply
        // don't offer this panel — matching how `effects_panel`'s clip-audio
        // actions are gated the same way.
        return;
    }
    sync_selected_clip(panel, clip_id);

    ui.horizontal(|ui| {
        if ui
            .button("Transcribe")
            .on_hover_text("local Whisper transcription of this clip's audio — fully offline, can take a while on long clips")
            .clicked()
        {
            panel.anchor = None;
            panel.end = None;
            let found = state.transcribe_clip(clip_id);
            if !found && state.status.is_empty() {
                state.status = "no speech found".into();
            }
        }
    });

    let Some(words) = state.transcripts.get(&clip_id) else {
        ui.weak("No transcript yet.");
        return;
    };
    if words.is_empty() {
        ui.weak("No speech detected in this clip.");
        return;
    }
    // Cloned so the click-handling loop below can call `&mut state` (to
    // seek the playhead) without fighting a live borrow of
    // `state.transcripts` — these are small (one clip's word list), so the
    // clone is not a real cost.
    let words = words.clone();

    let selection_range = match (panel.anchor, panel.end) {
        (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
        _ => None,
    };

    ui.separator();
    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for (i, word) in words.iter().enumerate() {
                let selected = selection_range.is_some_and(|(lo, hi)| i >= lo && i <= hi);
                let resp = ui.selectable_label(selected, &word.text);
                if resp.clicked() {
                    let shift_held = ui.input(|inp| inp.modifiers.shift);
                    if shift_held && panel.anchor.is_some() {
                        panel.end = Some(i);
                    } else {
                        panel.anchor = Some(i);
                        panel.end = Some(i);
                        state.playhead = word.start_tick;
                        state.playing = false;
                    }
                }
            }
        });
    });

    if let Some((lo, hi)) = selection_range {
        ui.horizontal(|ui| {
            let count = hi - lo + 1;
            if ui.button(format!("Delete {count} word{}", if count == 1 { "" } else { "s" })).clicked() {
                if state.delete_word_range(track_id, clip_id, lo, hi) {
                    state.status = format!("removed {count} word{}", if count == 1 { "" } else { "s" });
                    panel.anchor = None;
                    panel.end = None;
                    // The clip's own ticks (and the transcript mapped onto
                    // them) are now stale after the ripple — clearing avoids
                    // showing word positions that no longer match the
                    // timeline until the clip is re-transcribed.
                    state.transcripts.remove(&clip_id);
                } else {
                    state.status = "couldn't delete that range — it touches the clip's own edge".into();
                }
            }
            if ui.button("Clear selection").clicked() {
                panel.anchor = None;
                panel.end = None;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_selected_clip_keeps_the_selection_for_the_same_clip() {
        let mut panel = TranscriptPanelState { anchor: Some(2), end: Some(5), selected_clip: Some(ClipInstanceId(1)) };
        sync_selected_clip(&mut panel, ClipInstanceId(1));
        assert_eq!(panel.anchor, Some(2));
        assert_eq!(panel.end, Some(5));
    }

    #[test]
    fn sync_selected_clip_clears_the_selection_when_the_clip_changes() {
        let mut panel = TranscriptPanelState { anchor: Some(2), end: Some(5), selected_clip: Some(ClipInstanceId(1)) };
        sync_selected_clip(&mut panel, ClipInstanceId(2));
        assert_eq!(panel.anchor, None, "a selection made on clip 1 must not carry over to clip 2");
        assert_eq!(panel.end, None);
        assert_eq!(panel.selected_clip, Some(ClipInstanceId(2)));
    }

    #[test]
    fn sync_selected_clip_leaves_no_selection_alone_on_first_use() {
        let mut panel = TranscriptPanelState::default();
        sync_selected_clip(&mut panel, ClipInstanceId(1));
        assert_eq!(panel.anchor, None);
        assert_eq!(panel.end, None);
        assert_eq!(panel.selected_clip, Some(ClipInstanceId(1)));
    }
}
