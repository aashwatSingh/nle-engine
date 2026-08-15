//! Properties for the selected title clip: the text itself plus font, size,
//! colour, alignment, and position.
//!
//! Every widget writes through `EditorState::set_title_spec`, which rejects a
//! spec identical to the current one and folds a run of real changes into one
//! undo entry. That's what makes it safe for this panel to submit the whole
//! spec on every frame rather than tracking which field the user touched —
//! egui is immediate-mode, so "what changed" isn't information the panel has
//! without keeping a shadow copy of the model.

use crate::state::EditorState;
use timeline::{ClipInstanceId, ClipSource, TextAlign, TitleSpec};

/// Draws the panel for whichever selected clip is a title, if any. Returns
/// silently when the selection holds no title — a title-less selection is the
/// common case and shouldn't produce an empty header.
pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    let Some((id, mut spec)) = selected_title(state) else { return };

    egui::CollapsingHeader::new("Title")
        .default_open(true)
        .show(ui, |ui| {
            let mut changed = false;

            ui.label("Text");
            // Multiline, because a title with two lines is ordinary and
            // `text.rs` already stacks them around the anchor.
            changed |= ui
                .add(
                    egui::TextEdit::multiline(&mut spec.text)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                )
                .changed();

            ui.horizontal(|ui| {
                ui.label("Size");
                changed |= ui
                    .add(egui::DragValue::new(&mut spec.size_px).speed(1.0).range(4.0..=512.0))
                    .changed();
            });

            ui.horizontal(|ui| {
                ui.label("Font");
                // A free-text field rather than a dropdown of installed
                // families: the font index is keyed by file stem, not by the
                // display name a picker would need, so offering a list would
                // mean showing the user "segoeui" and calling it a font name.
                // Typing an unknown family falls back rather than failing —
                // see `render::text::FontLibrary::face`.
                changed |= ui
                    .add(egui::TextEdit::singleline(&mut spec.font_family).desired_width(140.0))
                    .changed();
            });

            ui.horizontal(|ui| {
                ui.label("Colour");
                let mut rgba = [
                    spec.color[0] as f32,
                    spec.color[1] as f32,
                    spec.color[2] as f32,
                    spec.color[3] as f32,
                ];
                if ui.color_edit_button_rgba_unmultiplied(&mut rgba).changed() {
                    spec.color = [rgba[0] as f64, rgba[1] as f64, rgba[2] as f64, rgba[3] as f64];
                    changed = true;
                }
            });

            ui.horizontal(|ui| {
                ui.label("Align");
                for (label, value) in
                    [("Left", TextAlign::Left), ("Centre", TextAlign::Center), ("Right", TextAlign::Right)]
                {
                    changed |= ui.selectable_value(&mut spec.align, value, label).changed();
                }
            });

            ui.horizontal(|ui| {
                ui.label("Position");
                // Fractions of the frame, so a title stays where it was put
                // when the sequence is re-sized or the project is opened at a
                // different resolution.
                changed |= ui
                    .add(egui::DragValue::new(&mut spec.position.0).speed(0.005).range(0.0..=1.0).prefix("x "))
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(&mut spec.position.1).speed(0.005).range(0.0..=1.0).prefix("y "))
                    .changed();
            });

            if changed {
                state.set_title_spec(id, spec);
            }
        });
}

/// The first selected clip that is a title, with a copy of its spec to edit.
fn selected_title(state: &EditorState) -> Option<(ClipInstanceId, TitleSpec)> {
    let selected = &state.selected_clips;
    state
        .sequence()
        .tracks
        .iter()
        .flat_map(|t| &t.clips)
        .find(|c| selected.contains(&c.id) && matches!(c.source, ClipSource::Title(_)))
        .map(|c| {
            let ClipSource::Title(spec) = &c.source else { unreachable!() };
            (c.id, spec.clone())
        })
}
