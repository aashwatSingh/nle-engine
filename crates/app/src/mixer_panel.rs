//! Audio mixer: one strip per audio track with a fader, pan, mute/solo, and a
//! live level meter, plus a master strip.
//!
//! ## What the meters show
//!
//! Levels come from `playback::SequenceAudioEngine::meters()`, which tags each
//! mixed block with the output position it covers so the reading matches what
//! the hardware is playing *now* — not what the mixer is working on several
//! hundred milliseconds ahead. Meters that lead the sound read as broken, so
//! this matters more than it sounds.
//!
//! Track meters are **post-fader, pre-master**: moving a track's fader moves
//! its meter, moving the master fader doesn't. That's the console convention,
//! and it's what makes the two meters together tell you *where* a level problem
//! is.
//!
//! When nothing is playing there is no signal to measure, so the meters sit at
//! the floor rather than holding their last value — a frozen meter is
//! indistinguishable from a stuck one.
//!
//! ## Scope
//!
//! No submixes, no insert effects (EQ/compressor/limiter), and no fader
//! automation. The first two need a real routing graph rather than the fixed
//! track-to-master path the mixer has; the third needs an automation lane in
//! the timeline. See `audio::timeline_mix`'s module doc.

use crate::state::EditorState;
use timeline::{TrackId, TrackKind};

/// Fader range. -60 dB is effectively off for a level control, and +6 gives a
/// little headroom to push a quiet track — matching a typical NLE mixer.
const FADER_MIN_DB: f64 = -60.0;
const FADER_MAX_DB: f64 = 6.0;

/// Meter scale. Below -60 dB the bar would be a sliver regardless, and the
/// scale has to match the fader's so the two read together.
const METER_FLOOR_DB: f32 = -60.0;
/// Above this, the meter turns red. -6 dBFS is the conventional "getting hot"
/// warning point, leaving headroom before actual clipping at 0.
const METER_HOT_DB: f32 = -6.0;

const METER_WIDTH: f32 = 10.0;
const METER_HEIGHT: f32 = 90.0;

pub fn show(ui: &mut egui::Ui, state: &mut EditorState, meters: Option<&playback::MeterSnapshot>) {
    ui.heading("Audio Mixer");

    let audio_tracks: Vec<(TrackId, String, f64, bool, f64, bool, bool)> = state
        .sequence()
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio)
        // The fader value is read at the playhead rather than taken from the
        // track, so an automated fader follows its curve as the playhead moves
        // — which is what makes automation visible rather than invisible.
        .map(|t| {
            (
                t.id,
                t.name.clone(),
                t.gain_db.evaluate_at(timeline::TimeTick(state.playhead)).as_scalar().unwrap_or(0.0),
                t.gain_db.is_animated(),
                t.pan,
                t.muted,
                t.solo,
            )
        })
        .collect();

    if audio_tracks.is_empty() {
        ui.label("No audio tracks yet — add a clip with sound to the timeline.");
        return;
    }

    egui::ScrollArea::horizontal().show(ui, |ui| {
        ui.horizontal(|ui| {
            for (id, name, gain_db, automated, pan, muted, solo) in &audio_tracks {
                let level = meters
                    .and_then(|m| m.tracks.iter().find(|(t, _)| t == id))
                    .map(|(_, m)| *m);
                strip(ui, state, Some(*id), name, *gain_db, *automated, *pan, *muted, *solo, level);
                ui.separator();
            }
            // Master last, on the right, like every console.
            let master_gain = state.master_gain_db;
            strip(
                ui,
                state,
                None,
                "Master",
                master_gain,
                // The master bus isn't part of the project model (it's a
                // `MixOptions` value), so it has nowhere to keep keyframes and
                // is never automated.
                false,
                0.0,
                false,
                false,
                meters.map(|m| m.master),
            );
        });
    });
}

/// One mixer strip. `track` is `None` for the master bus, which has a fader and
/// a meter but no pan, mute, or solo — those are per-track concepts.
#[allow(clippy::too_many_arguments)]
fn strip(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    track: Option<TrackId>,
    name: &str,
    gain_db: f64,
    // Whether this fader is following keyframes rather than sitting where it
    // was last put. Changes what dragging it means — see `set_track_gain`.
    automated: bool,
    pan: f64,
    muted: bool,
    solo: bool,
    level: Option<audio::PeakRms>,
) {
    ui.vertical(|ui| {
        ui.set_width(96.0);
        ui.strong(name);

        ui.horizontal(|ui| {
            draw_meter(ui, level);
            let mut db = gain_db;
            // Vertical, like a real fader — and it makes the strip readable
            // beside its meter.
            let resp = ui.add(
                egui::Slider::new(&mut db, FADER_MIN_DB..=FADER_MAX_DB)
                    .vertical()
                    .show_value(false),
            );
            if resp.drag_started() {
                state.begin_drag_edit("fader");
            }
            if resp.changed() {
                match track {
                    Some(id) => state.set_track_gain(id, db, state.coalescing_open),
                    None => state.master_gain_db = db,
                }
            }
            if resp.drag_stopped() && state.coalescing_open {
                state.end_drag_edit();
            }
        });

        // The numeric readout matters: "about there" is not a mix decision, and
        // a fader with no number can't be matched between two tracks.
        ui.label(format!("{gain_db:+.1} dB"));

        // Automation controls, tracks only — the master has nowhere to keep
        // keyframes (see the call site).
        if let Some(id) = track {
            ui.horizontal(|ui| {
                if automated {
                    // A filled marker when the playhead is *on* a keyframe,
                    // hollow when it's between them: the difference decides
                    // whether the button below adds or removes one, so it has
                    // to be visible before the click, not after.
                    let on_key = state.track_gain_keyframe_at_playhead(id);
                    let (glyph, hint): (&str, &str) = if on_key {
                        ("[K]", "remove the fader keyframe at the playhead")
                    } else {
                        ("[+]", "add a fader keyframe at the playhead")
                    };
                    if ui.small_button(glyph).on_hover_text(hint).clicked() {
                        if on_key {
                            state.remove_track_gain_keyframe(id);
                        } else {
                            state.add_track_gain_keyframe(id);
                        }
                    }
                    ui.weak("auto");
                } else if ui
                    .small_button("auto")
                    .on_hover_text("start automating this fader: adds a keyframe at the playhead")
                    .clicked()
                {
                    state.add_track_gain_keyframe(id);
                }
            });
        }
        if let Some(l) = level {
            ui.weak(format!("pk {:.1}", l.peak_dbfs.max(METER_FLOOR_DB)));
        } else {
            ui.weak("pk --");
        }

        if let Some(id) = track {
            let mut p = pan;
            let resp = ui.add(egui::Slider::new(&mut p, -1.0..=1.0).show_value(false).text("pan"));
            if resp.changed() {
                state.set_track_pan(id, p);
            }
            ui.horizontal(|ui| {
                let mut m = muted;
                if ui.toggle_value(&mut m, "M").on_hover_text("mute").clicked() {
                    state.set_track_mute(id, m);
                }
                let mut s = solo;
                if ui.toggle_value(&mut s, "S").on_hover_text("solo").clicked() {
                    state.set_track_solo(id, s);
                }
            });
        }
    });
}

/// A peak-over-RMS bar: the filled body is RMS (average level), the bright line
/// is peak. Showing only one of the two is the usual mistake — RMS alone hides
/// transients that clip, peak alone gives no sense of loudness.
fn draw_meter(ui: &mut egui::Ui, level: Option<audio::PeakRms>) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(METER_WIDTH, METER_HEIGHT),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    painter.rect_filled(rect, 1.0, egui::Color32::from_gray(28));

    let Some(level) = level else { return };
    let frac = |db: f32| ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);

    let rms_h = rect.height() * frac(level.rms_dbfs);
    if rms_h > 0.0 {
        let colour = if level.peak_dbfs >= METER_HOT_DB {
            egui::Color32::from_rgb(220, 90, 60)
        } else {
            egui::Color32::from_rgb(90, 190, 120)
        };
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.bottom() - rms_h),
                rect.right_bottom(),
            ),
            1.0,
            colour,
        );
    }

    let peak_y = rect.bottom() - rect.height() * frac(level.peak_dbfs);
    if level.peak_dbfs > METER_FLOOR_DB {
        painter.line_segment(
            [egui::pos2(rect.left(), peak_y), egui::pos2(rect.right(), peak_y)],
            egui::Stroke::new(1.5f32, egui::Color32::WHITE),
        );
    }
}
