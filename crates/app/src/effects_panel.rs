//! Effects panel: shows the selected clip's effect stack, lets you add from
//! the five built-in effects (M5), adjust their params, and **animate them**.
//!
//! ## Keyframing
//!
//! `timeline::ParamTrack` has always supported keyframes with hold / linear /
//! bezier / auto-bezier interpolation, and `render::graph` has always
//! evaluated them per frame — but nothing in the UI could create one, so the
//! entire animation engine was unreachable and every parameter was in
//! practice a constant. This is the surface for it, following Premiere's
//! model:
//!
//! - A stopwatch (`[o]` / `(o)`) toggles animation per parameter. Off means
//!   the track has no keyframes and reads `default`.
//! - With animation on, editing the value writes a keyframe **at the
//!   playhead** rather than changing the constant.
//! - `<` / `>` jump to the previous/next keyframe; the diamond adds or removes
//!   one at the playhead.
//! - A strip under the control shows keyframes across the clip's duration.
//!   Click to move the playhead to one, drag to retime it, right-click for its
//!   interpolation mode.
//! - A `Bezier` keyframe (not `AutoBezier` — see below) additionally shows two
//!   small square tangent handles, draggable left/right to adjust ease timing.
//!
//! Keyframe times are **clip-relative** (see
//! `EditorState::playhead_local_to_clip`), so the controls are inert when the
//! playhead isn't over the selected clip — there's no meaningful time to write
//! to. The panel says so rather than silently doing nothing.
//!
//! ## Tangent handles are timing-only
//!
//! The strip has no vertical value axis — it's a flat timeline, like most
//! NLEs' audio keyframe lanes — so a handle can only be dragged horizontally.
//! That edits the **time** component of `Keyframe::tangents`
//! (`(time-ticks-delta, value-delta)`); the value-delta component is left at
//! whatever it already was (`0.0` for a freshly-seeded handle), which makes
//! this a pure ease-width control rather than a curve-shape one. A real
//! value-vs-time curve graph is what full 2D tangent editing needs, and stays
//! a separate, larger gap — this closes the "the engine supports it but
//! nothing can reach it" gap without pretending to be that graph.
//!
//! Only `Bezier` shows handles, not `AutoBezier`: `evaluate_at`'s fallback
//! match ignores stored `tangents` entirely for `AutoBezier` (it always uses
//! the computed auto-tangent), so dragging a handle there would silently do
//! nothing audible — worse than not offering the drag at all.
//!
//! Effect params aren't part of the clip-position invariants
//! `timeline::edit_ops` enforces (no overlap/gap checking applies to them),
//! so — like `EditorState::import_assets` — these edits clone-mutate-push
//! the project directly rather than going through an `EditOp`. There's no
//! "SetEffectParam" op because nothing about it needs the invariant-checked
//! machinery that op set exists for.

use crate::state::{AnalysisKind, EditorState};
use render::{chroma_key, color_correction, crop, gaussian_blur, mask, transform, ParamSchema, ParamType};
use timeline::{
    ClipInstanceId, EffectInstance, EffectInstanceId, InterpolationMode, ParamValue, TimeTick,
};

/// Height of the per-parameter keyframe strip.
const STRIP_HEIGHT: f32 = 14.0;
/// Half-width of a keyframe diamond, and its click tolerance.
const DIAMOND_R: f32 = 4.5;

/// One row of the keyframe strip: when it sits, how it interpolates, and
/// its bezier handles when it has explicit ones.
type KeyframeRow = (TimeTick, InterpolationMode, Option<((f64, f64), (f64, f64))>);

/// Which keyframe is mid-drag, if any. Keyed by everything needed to find it
/// again next frame, because the strip is rebuilt from scratch each repaint.
#[derive(Clone)]
pub struct KeyframeDrag {
    clip: ClipInstanceId,
    effect: EffectInstanceId,
    param: String,
    /// Current time of the dragged keyframe — updated as it moves, so the next
    /// frame knows where to find it.
    at: TimeTick,
}

#[derive(Clone, Copy, PartialEq)]
enum TangentSide {
    In,
    Out,
}

/// Which tangent handle is mid-drag, if any.
#[derive(Clone)]
struct TangentDrag {
    clip: ClipInstanceId,
    effect: EffectInstanceId,
    param: String,
    /// The keyframe the handle belongs to. Fixed for the drag's duration —
    /// unlike a keyframe move, dragging a tangent never changes which
    /// keyframe owns it.
    keyframe_at: TimeTick,
    side: TangentSide,
    /// Pointer tick minus handle tick at drag start, so the handle doesn't
    /// jump to re-center under the pointer on the first move event — the same
    /// fix `MoveClip` uses in the timeline widget.
    grab_offset_ticks: i64,
}

pub struct EffectsPanelState {
    drag: Option<KeyframeDrag>,
    tangent_drag: Option<TangentDrag>,
    /// The "Match Loudness" target field's value, kept across frames since
    /// it's an ordinary text/drag input the user sets once and reuses.
    target_lufs: f64,
}

impl Default for EffectsPanelState {
    fn default() -> Self {
        EffectsPanelState {
            drag: None,
            tangent_drag: None,
            // -14 LUFS: the common streaming-platform integrated-loudness
            // target (Spotify, YouTube, most social platforms land close to
            // this), and a reasonable default for "make this clip sit at a
            // normal level" rather than a broadcast (-23 LUFS) or podcast
            // (-16 to -19 LUFS) target the user would have a specific reason
            // to want instead.
            target_lufs: -14.0,
        }
    }
}

/// The "Add Effect" menu's list. Deliberately its own hand-written list
/// rather than iterating `render::BuiltinRegistry` — the registry has no
/// ordering guarantee and menu order is a UI decision, not a lookup-table
/// one. That means adding an effect requires updating both this and the
/// registry; `add_effect_menu_matches_the_effect_registry` exists so that
/// drifting apart fails a test instead of shipping an effect nobody can add,
/// or an "Add Effect" entry that resolves to nothing.
fn available_effects() -> Vec<render::EffectDescriptor> {
    vec![
        transform::descriptor(),
        gaussian_blur::descriptor(),
        color_correction::descriptor(),
        crop::descriptor(),
        mask::descriptor(),
        chroma_key::descriptor(),
    ]
}

/// One clip-analysis action: a button while idle, a spinner saying what it's
/// doing while its background job runs. `target_lufs` is only read by
/// `AnalysisKind::Loudness`.
fn analysis_button(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    clip_id: ClipInstanceId,
    kind: AnalysisKind,
    label: &str,
    hover: &str,
    target_lufs: f64,
) {
    if state.analysis_running(clip_id, kind) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(kind.running_label());
        });
    } else if ui.button(label).on_hover_text(hover).clicked() {
        state.start_analysis(clip_id, kind, target_lufs);
    }
}

pub fn show(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    panel: &mut EffectsPanelState,
    matting_jobs: &mut crate::matting_jobs::MattingJobs,
) {
    ui.heading("Effects");
    let Some(clip_id) = state.primary_selection() else {
        ui.label("Select a clip to edit its effects.");
        return;
    };
    let Some((_, clip)) = state.find_clip(clip_id) else {
        ui.label("Selected clip no longer exists.");
        return;
    };

    ui.menu_button("Add Effect", |ui| {
        for desc in available_effects() {
            if ui.button(desc.display_name).clicked() {
                add_effect(state, clip_id, &desc);
                ui.close_menu();
            }
        }
    });

    if matches!(clip.source, timeline::ClipSource::Media(_)) {
        // Each of these decodes the whole clip, which on long footage takes
        // minutes. They run as background jobs (`EditorState::start_analysis`)
        // so the editor stays usable meanwhile; the result lands as one undo
        // step when it's ready.
        let target_lufs = panel.target_lufs;
        analysis_button(
            ui,
            state,
            clip_id,
            AnalysisKind::SceneCuts,
            "Detect Scene Cuts",
            "splits this clip at hard cuts — analyses the whole clip in the background",
            target_lufs,
        );
        analysis_button(
            ui,
            state,
            clip_id,
            AnalysisKind::Silence,
            "Remove Silence",
            "ripple-deletes pauses below -40dBFS longer than 0.3s — decodes the whole clip's audio in the background",
            target_lufs,
        );
        analysis_button(
            ui,
            state,
            clip_id,
            AnalysisKind::Beats,
            "Detect Beats",
            "adds a snap-to marker at each detected beat in this clip's audio — runs in the background",
            target_lufs,
        );
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut panel.target_lufs).speed(0.5).suffix(" LUFS"));
            analysis_button(
                ui,
                state,
                clip_id,
                AnalysisKind::Loudness,
                "Match Loudness",
                "measures this clip's real EBU R128 loudness and sets its gain to reach the target",
                panel.target_lufs,
            );
        });
        analysis_button(
            ui,
            state,
            clip_id,
            AnalysisKind::Stabilize,
            "Stabilize",
            "corrects handheld pan/shake by keyframing position — translation only, no rotation/zoom/crop; runs in the background",
            target_lufs,
        );
        analysis_button(
            ui,
            state,
            clip_id,
            AnalysisKind::Captions,
            "Generate Captions",
            "transcribes this clip's audio with local Whisper and places a title per line — fully offline, no data leaves this machine; runs in the background",
            target_lufs,
        );

        if let timeline::ClipSource::Media(asset_id) = clip.source {
            ui.horizontal(|ui| {
                use crate::matting_jobs::MattingState;
                match matting_jobs.state_of(asset_id) {
                    MattingState::None => {
                        if ui
                            .button("Remove Background")
                            .on_hover_text("AI background removal (local model, no data leaves this machine) — runs in the background, applies to every clip using this same source file")
                            .clicked()
                        {
                            if let Some(path) = state.asset_paths.get(&asset_id).cloned() {
                                if matting_jobs.request(asset_id, path) {
                                    state.status = "removing background in the background — this can take a while".into();
                                }
                            } else {
                                state.status = "can't remove background — source file not found".into();
                            }
                        }
                    }
                    MattingState::Building => {
                        ui.add_enabled(false, egui::Button::new("Removing Background…"));
                    }
                    MattingState::Ready => {
                        ui.weak("Background removed ✓");
                    }
                    MattingState::Failed => {
                        ui.colored_label(egui::Color32::LIGHT_RED, "Background removal failed");
                        if let Some(err) = matting_jobs.error_for(asset_id) {
                            ui.weak(err).on_hover_text(err);
                        }
                    }
                }
            });
        }
    }

    // Keyframing needs a time inside the clip to write to. Say why the
    // controls are inert instead of leaving them looking broken.
    let local = state.playhead_local_to_clip(clip_id);
    if local.is_none() {
        ui.colored_label(
            egui::Color32::from_rgb(255, 170, 60),
            "playhead is outside this clip — move it over the clip to keyframe",
        );
    }
    ui.separator();

    egui::ScrollArea::vertical().show(ui, |ui| {
        for effect in &clip.effects {
            let Some(desc) = available_effects()
                .into_iter()
                .find(|d| d.type_id == effect.effect_type)
            else {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("unknown effect: {}", effect.effect_type),
                );
                continue;
            };
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.strong(desc.display_name);
                    if ui.small_button("remove").clicked() {
                        state.remove_effect(clip_id, effect.id);
                    }
                });
                for param in &desc.params {
                    // Params are addressed by name within an effect instance,
                    // so IDs derived from layout would collide between two
                    // instances of the same effect type on one clip.
                    ui.push_id((effect.id.0, param.name), |ui| {
                        show_param(ui, state, panel, clip_id, effect.id, param, local);
                    });
                }
            });
        }
        if clip.effects.is_empty() {
            ui.label("No effects on this clip.");
        }
    });
}

/// The stopwatch + keyframe navigation row for one parameter.
///
/// Returns nothing; all effects go through `state`. Drawn above the value
/// control so the control's own width isn't disturbed by it.
fn keyframe_controls(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    clip_id: ClipInstanceId,
    effect_id: EffectInstanceId,
    param: &ParamSchema,
    local: Option<TimeTick>,
) {
    let Some((_, clip)) = state.find_clip(clip_id) else { return };
    let Some(effect) = clip.effects.iter().find(|e| e.id == effect_id) else { return };
    let Some(track) = effect.params.get(param.name) else { return };
    let animated = track.is_animated();
    let (prev, next) = match local {
        Some(l) => (track.prev_keyframe_before(l), track.next_keyframe_after(l)),
        None => (None, None),
    };
    let on_keyframe = local.and_then(|l| track.keyframe_index_at(l)).is_some();
    let count = track.keyframes.len();

    ui.horizontal(|ui| {
        // ASCII only: the bundled egui font has no stopwatch/diamond glyphs,
        // and a missing glyph renders as tofu (learned the hard way in the
        // Project panel).
        let stopwatch = if animated { "(o)" } else { "[o]" };
        let hint = if animated {
            "animation on — click to collapse to a constant at the playhead"
        } else {
            "animate this parameter (keyframes)"
        };
        if ui
            .add_enabled(local.is_some(), egui::Button::new(stopwatch).small())
            .on_hover_text(hint)
            .clicked()
        {
            if let Some(l) = local {
                state.toggle_param_animation(clip_id, effect_id, param.name, l);
            }
        }
        ui.label(param.display_name);

        if animated {
            if ui
                .add_enabled(prev.is_some(), egui::Button::new("<").small())
                .on_hover_text("previous keyframe")
                .clicked()
            {
                if let Some(p) = prev {
                    state.playhead = clip.timeline_in.0 + p.0;
                }
            }
            let diamond = if on_keyframe { "*" } else { "+" };
            if ui
                .add_enabled(local.is_some(), egui::Button::new(diamond).small())
                .on_hover_text(if on_keyframe {
                    "remove keyframe at playhead"
                } else {
                    "add keyframe at playhead"
                })
                .clicked()
            {
                if let Some(l) = local {
                    state.toggle_keyframe(clip_id, effect_id, param.name, l);
                }
            }
            if ui
                .add_enabled(next.is_some(), egui::Button::new(">").small())
                .on_hover_text("next keyframe")
                .clicked()
            {
                if let Some(n) = next {
                    state.playhead = clip.timeline_in.0 + n.0;
                }
            }
            ui.weak(format!("{count} kf"));
        }
    });
}

/// Keyframe strip: positions across the clip's duration, draggable to retime,
/// right-clickable for interpolation mode.
fn keyframe_strip(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    panel: &mut EffectsPanelState,
    clip_id: ClipInstanceId,
    effect_id: EffectInstanceId,
    param: &ParamSchema,
) {
    let Some((_, clip)) = state.find_clip(clip_id) else { return };
    let Some(effect) = clip.effects.iter().find(|e| e.id == effect_id) else { return };
    let Some(track) = effect.params.get(param.name) else { return };
    if !track.is_animated() {
        return;
    }
    let clip_in = clip.timeline_in.0;
    let clip_len = (clip.timeline_out.0 - clip_in).max(1);
    let keyframes: Vec<KeyframeRow> = track
        .keyframes
        .iter()
        .map(|k| (k.at, k.interpolation, k.tangents))
        .collect();

    let width = ui.available_width().max(60.0);
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(width, STRIP_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(38));

    let x_of = |t: TimeTick| rect.left() + (t.0 as f32 / clip_len as f32) * rect.width();
    let tick_of = |x: f32| {
        let frac = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        TimeTick((frac as f64 * clip_len as f64) as i64)
    };

    // Playhead marker, so "add keyframe here" has a visible here.
    if state.playhead >= clip_in && state.playhead < clip.timeline_out.0 {
        let px = x_of(TimeTick(state.playhead - clip_in));
        painter.line_segment(
            [egui::pos2(px, rect.top()), egui::pos2(px, rect.bottom())],
            egui::Stroke::new(1.0f32, egui::Color32::from_rgb(120, 200, 255)),
        );
    }

    // Default tangent offset for a Bezier keyframe with no explicit tangents
    // yet: a third of the distance to the neighbouring keyframe on that side,
    // or a fixed fallback at either end of the track. This is only ever used
    // to give a fresh handle a sensible starting position to grab and to seed
    // the first drag frame — the engine's own fallback (`auto_tangents`) is
    // what actually renders until a drag writes something explicit, and the
    // two are not required to match exactly.
    let default_offset = |at: TimeTick, want_next: bool| -> i64 {
        let neighbour = if want_next {
            keyframes.iter().map(|(a, ..)| *a).find(|a| a.0 > at.0)
        } else {
            keyframes.iter().map(|(a, ..)| *a).filter(|a| a.0 < at.0).max()
        };
        let magnitude = match neighbour {
            Some(n) => ((n.0 - at.0).abs() / 3).max(1),
            None => (clip_len / 10).max(1),
        };
        if want_next { magnitude } else { -magnitude }
    };

    for (at, mode, tangents) in &keyframes {
        let c = egui::pos2(x_of(*at), rect.center().y);
        // Hold keyframes drawn square — the shape difference is how NLEs show
        // "this segment doesn't interpolate" at a glance.
        let fill = egui::Color32::from_rgb(230, 200, 120);
        if *mode == InterpolationMode::Hold {
            painter.rect_filled(
                egui::Rect::from_center_size(c, egui::vec2(DIAMOND_R * 1.6, DIAMOND_R * 1.6)),
                0.0,
                fill,
            );
        } else {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x, c.y - DIAMOND_R),
                    egui::pos2(c.x + DIAMOND_R, c.y),
                    egui::pos2(c.x, c.y + DIAMOND_R),
                    egui::pos2(c.x - DIAMOND_R, c.y),
                ],
                fill,
                egui::Stroke::NONE,
            ));
        }

        // Tangent handles: Bezier only — see the module doc on why AutoBezier
        // doesn't get them (its stored tangents, if any, are never read).
        if *mode == InterpolationMode::Bezier {
            let (in_dx, out_dx) = match tangents {
                Some((in_t, out_t)) => (in_t.0 as i64, out_t.0 as i64),
                None => (default_offset(*at, false), default_offset(*at, true)),
            };
            for (dx, colour) in
                [(in_dx, egui::Color32::from_rgb(140, 200, 255)), (out_dx, egui::Color32::from_rgb(140, 255, 180))]
            {
                let hx = x_of(TimeTick(at.0 + dx));
                painter.line_segment([c, egui::pos2(hx, c.y)], egui::Stroke::new(1.0f32, colour));
                painter.rect_filled(
                    egui::Rect::from_center_size(egui::pos2(hx, c.y), egui::vec2(5.0, 5.0)),
                    0.0,
                    colour,
                );
            }
        }
    }

    let nearest = |x: f32| -> Option<TimeTick> {
        keyframes
            .iter()
            .map(|(at, ..)| *at)
            .filter(|at| (x_of(*at) - x).abs() <= DIAMOND_R * 2.0)
            .min_by_key(|at| (x_of(*at) - x).abs() as i64)
    };

    // Nearest tangent handle to `x`, among Bezier keyframes only, within its
    // own (tighter) hit tolerance so a drag near a keyframe still prefers the
    // keyframe itself over a handle sitting further out.
    let nearest_handle = |x: f32| -> Option<(TimeTick, TangentSide, i64)> {
        keyframes
            .iter()
            .filter(|(_, mode, _)| *mode == InterpolationMode::Bezier)
            .flat_map(|(at, _, tangents)| {
                let (in_dx, out_dx) = match tangents {
                    Some((in_t, out_t)) => (in_t.0 as i64, out_t.0 as i64),
                    None => (default_offset(*at, false), default_offset(*at, true)),
                };
                [(*at, TangentSide::In, in_dx), (*at, TangentSide::Out, out_dx)]
            })
            .map(|(at, side, dx)| (at, side, dx, x_of(TimeTick(at.0 + dx))))
            .filter(|(.., hx)| (hx - x).abs() <= DIAMOND_R * 1.5)
            .min_by_key(|(.., hx)| (hx - x).abs() as i64)
            .map(|(at, side, dx, _)| (at, side, dx))
    };

    if resp.drag_started() {
        let pos = resp.interact_pointer_pos();
        // Handles take priority only when no keyframe itself is under the
        // pointer — the keyframe's own hit zone is the more common gesture,
        // and its tolerance is generous enough to shadow a handle only when
        // they're nearly on top of each other.
        if let Some((at, side, dx)) = pos.filter(|p| nearest(p.x).is_none()).and_then(|p| nearest_handle(p.x)) {
            let handle_tick = tick_of(egui::pos2(x_of(TimeTick(at.0 + dx)), 0.0).x);
            let grab_offset_ticks = tick_of(pos.unwrap().x).0 - handle_tick.0;
            state.begin_drag_edit("edit tangent");
            panel.tangent_drag = Some(TangentDrag {
                clip: clip_id,
                effect: effect_id,
                param: param.name.to_string(),
                keyframe_at: at,
                side,
                grab_offset_ticks,
            });
        } else if let Some(at) = pos.and_then(|p| nearest(p.x)) {
            state.begin_drag_edit("move keyframe");
            panel.drag = Some(KeyframeDrag {
                clip: clip_id,
                effect: effect_id,
                param: param.name.to_string(),
                at,
            });
        }
    }

    if resp.dragged() {
        // Only act on a drag this strip actually owns — every parameter draws
        // its own strip, and they all see the same pointer events.
        let owns_tangent = panel.tangent_drag.as_ref().is_some_and(|d| {
            d.clip == clip_id && d.effect == effect_id && d.param == param.name
        });
        let owns_keyframe = panel.drag.as_ref().is_some_and(|d| {
            d.clip == clip_id && d.effect == effect_id && d.param == param.name
        });
        if owns_tangent {
            if let Some(pos) = resp.interact_pointer_pos() {
                let d = panel.tangent_drag.as_ref().unwrap().clone();
                let new_handle_tick = tick_of(pos.x).0 - d.grab_offset_ticks;
                let mut new_dx = new_handle_tick - d.keyframe_at.0;
                // A handle that crossed to the wrong side of its keyframe
                // would flip which end of the curve it shapes — clamp to a
                // minimum one-tick magnitude on the correct side instead.
                new_dx = match d.side {
                    TangentSide::In => new_dx.min(-1),
                    TangentSide::Out => new_dx.max(1),
                };

                let current = track.keyframes.iter().find(|k| k.at == d.keyframe_at).and_then(|k| k.tangents);
                let (mut in_t, mut out_t) = current.unwrap_or((
                    (default_offset(d.keyframe_at, false) as f64, 0.0),
                    (default_offset(d.keyframe_at, true) as f64, 0.0),
                ));
                match d.side {
                    TangentSide::In => in_t.0 = new_dx as f64,
                    TangentSide::Out => out_t.0 = new_dx as f64,
                }
                state.set_keyframe_tangents(clip_id, effect_id, param.name, d.keyframe_at, (in_t, out_t));
            }
        } else if owns_keyframe {
            if let Some(pos) = resp.interact_pointer_pos() {
                let from = panel.drag.as_ref().unwrap().at;
                let to = tick_of(pos.x);
                if to != from {
                    state.move_keyframe(clip_id, effect_id, param.name, from, to);
                    // Track where it landed, or the next move event would look
                    // for it at a time it no longer occupies and do nothing.
                    if let Some(d) = panel.drag.as_mut() {
                        d.at = to;
                    }
                }
            }
        }
    }

    if resp.drag_stopped() {
        if (panel.drag.is_some() || panel.tangent_drag.is_some()) && state.coalescing_open() {
            state.end_drag_edit();
        }
        panel.drag = None;
        panel.tangent_drag = None;
    }

    // A plain click (no drag) parks the playhead on the keyframe, which is how
    // you get the value control to show that keyframe's value.
    if resp.clicked() {
        if let Some(at) = resp.interact_pointer_pos().and_then(|p| nearest(p.x)) {
            state.playhead = clip_in + at.0;
        }
    }

    resp.context_menu(|ui| {
        let Some(at) = ui
            .ctx()
            .pointer_interact_pos()
            .or(ui.ctx().pointer_latest_pos())
            .and_then(|p| nearest(p.x))
        else {
            ui.label("right-click a keyframe to set its interpolation");
            return;
        };
        ui.label("Interpolation (segment after this keyframe)");
        for (mode, name) in [
            (InterpolationMode::Hold, "Hold"),
            (InterpolationMode::Linear, "Linear"),
            (InterpolationMode::AutoBezier, "Auto Bezier (ease)"),
            (InterpolationMode::Bezier, "Bezier"),
        ] {
            if ui.button(name).clicked() {
                state.set_keyframe_interpolation(clip_id, effect_id, param.name, at, mode);
                ui.close_menu();
            }
        }
        ui.separator();
        if ui.button("Delete keyframe").clicked() {
            state.toggle_keyframe(clip_id, effect_id, param.name, at);
            ui.close_menu();
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn show_param(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    panel: &mut EffectsPanelState,
    clip_id: ClipInstanceId,
    effect_id: EffectInstanceId,
    param: &ParamSchema,
    local: Option<TimeTick>,
) {
    keyframe_controls(ui, state, clip_id, effect_id, param, local);

    let Some((_, clip)) = state.find_clip(clip_id) else {
        return;
    };
    let Some(effect) = clip.effects.iter().find(|e| e.id == effect_id) else {
        return;
    };
    let Some(track) = effect.params.get(param.name) else {
        return;
    };

    // Show the value the renderer would actually use at the playhead. Showing
    // `default` on an animated track would display a number that has no effect
    // on the picture, since `evaluate_at` ignores `default` entirely once any
    // keyframe exists.
    let current = match (track.is_animated(), local) {
        (true, Some(l)) => track.evaluate_at(l),
        (true, None) => track.evaluate_at(TimeTick(0)),
        (false, _) => track.default,
    };
    // A slider that writes nowhere is worse than a disabled one.
    let editable = !track.is_animated() || local.is_some();

    match (param.param_type, current) {
        (ParamType::Number, ParamValue::Number(v)) => {
            let mut v = v;
            let range = param.range.unwrap_or((0.0, 1.0));
            let resp = ui.add_enabled(
                editable,
                egui::Slider::new(&mut v, range.0..=range.1).text(param.display_name),
            );
            handle_change(
                state,
                &resp,
                clip_id,
                effect_id,
                param.name,
                ParamValue::Number(v),
                local,
            );
        }
        (ParamType::Vec2, ParamValue::Vec2(x, y)) => {
            let (mut x, mut y) = (x, y);
            ui.horizontal(|ui| {
                let rx =
                    ui.add_enabled(editable, egui::DragValue::new(&mut x).speed(1.0).prefix("x: "));
                handle_change(
                    state,
                    &rx,
                    clip_id,
                    effect_id,
                    param.name,
                    ParamValue::Vec2(x, y),
                    local,
                );
                let ry =
                    ui.add_enabled(editable, egui::DragValue::new(&mut y).speed(1.0).prefix("y: "));
                handle_change(
                    state,
                    &ry,
                    clip_id,
                    effect_id,
                    param.name,
                    ParamValue::Vec2(x, y),
                    local,
                );
            });
        }
        (ParamType::Color, ParamValue::Color(c)) => {
            let mut rgba = c;
            ui.horizontal(|ui| {
                ui.label(param.display_name);
                // A single discrete edit per colour pick, like the `Bool`
                // branch below — not routed through `handle_change`'s
                // drag-coalescing, which is calibrated for `Slider`/
                // `DragValue`'s `drag_started`/`drag_stopped` semantics that
                // a colour-wheel popup doesn't have reason to match.
                // `add_enabled_ui`, not `add_enabled`: the colour button is a
                // `Ui` method, not a `Widget` value `add_enabled` could take.
                let resp = ui
                    .add_enabled_ui(editable, |ui| ui.color_edit_button_rgba_unmultiplied(&mut rgba))
                    .inner;
                if resp.changed() {
                    state.set_param_value_at(
                        clip_id,
                        effect_id,
                        param.name,
                        ParamValue::Color(rgba),
                        local,
                        false,
                    );
                }
            });
        }
        (ParamType::Bool, ParamValue::Bool(b)) => {
            let mut b = b;
            if ui
                .add_enabled(editable, egui::Checkbox::new(&mut b, param.display_name))
                .changed()
            {
                state.set_param_value_at(
                    clip_id,
                    effect_id,
                    param.name,
                    ParamValue::Bool(b),
                    local,
                    false,
                );
            }
        }
        _ => {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!("{}: type mismatch", param.display_name),
            );
        }
    }

    keyframe_strip(ui, state, panel, clip_id, effect_id, param);
}

/// Sliders/drag-values fire `changed()` continuously while dragged and also
/// on a single click/keyboard nudge; only the former should coalesce into
/// one undo step, matching the timeline drag gestures' convention.
#[allow(clippy::too_many_arguments)]
fn handle_change(
    state: &mut EditorState,
    resp: &egui::Response,
    clip_id: ClipInstanceId,
    effect_id: EffectInstanceId,
    param_name: &str,
    new_value: ParamValue,
    local: Option<TimeTick>,
) {
    if resp.drag_started() {
        state.begin_drag_edit("edit effect");
    }
    if resp.changed() {
        state.set_param_value_at(
            clip_id,
            effect_id,
            param_name,
            new_value,
            local,
            state.coalescing_open(),
        );
    }
    if resp.drag_stopped() && state.coalescing_open() {
        state.end_drag_edit();
    }
}

fn add_effect(state: &mut EditorState, clip_id: ClipInstanceId, desc: &render::EffectDescriptor) {
    let mut params = std::collections::BTreeMap::new();
    for p in &desc.params {
        params.insert(
            p.name.to_string(),
            timeline::ParamTrack::constant(p.default),
        );
    }
    let effect = EffectInstance {
        id: EffectInstanceId(state.next_id()),
        effect_type: desc.type_id.to_string(),
        enabled: true,
        params,
    };
    state.add_effect(clip_id, effect);
}

#[cfg(test)]
mod effects_panel_tests {
    use super::available_effects;
    use render::{BuiltinRegistry, EffectRegistry};

    #[test]
    fn add_effect_menu_matches_the_effect_registry() {
        let registry = BuiltinRegistry::default();
        for desc in available_effects() {
            assert!(
                registry.lookup(desc.type_id).is_some(),
                "\"{}\" is offered in the Add Effect menu but isn't in BuiltinRegistry — \
                 adding it would create an effect instance the compositor doesn't recognise",
                desc.display_name
            );
        }
        // And the reverse: every registered effect should be reachable from
        // the menu, or it's built but nobody can ever add one to a clip.
        // Drawn from the registry itself, not a hand-copied id list, so this
        // check can't independently drift the same way the thing it's
        // guarding against can.
        for type_id in registry.all_type_ids() {
            assert!(
                available_effects().iter().any(|d| d.type_id == type_id),
                "\"{type_id}\" is in BuiltinRegistry but missing from the Add Effect menu"
            );
        }
    }
}
