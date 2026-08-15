//! The visible, editable timeline: a ruler, track lanes, and clips you can
//! click, drag, and trim. This is the piece that turns the engine (M0-M5)
//! into something you actually edit with, rather than a hardcoded demo.
//!
//! Every gesture here bottoms out in a `timeline::EditOp` applied through
//! `EditorState` — this file never mutates a `Project` directly. Dragging a
//! clip is `Lift` + `Overwrite` composed live (see `EditorState::move_clip`);
//! dragging an edge is `TrimRipple`; a Razor-tool click is `EditOp::Razor`.
//! Each drag coalesces into one undo step (spec 4.3: "dragging a clip is
//! one undo step, not 400").

use crate::icons::{icon_button, icon_toggle, Icon};
use crate::state::{Drag, EditorState, Tool};
use crate::waveform_cache::{column_range, WaveformCache};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use timeline::{ClipInstance, ClipSource, EditOp, TimeTick, TrackId, TrackKind, TIMEBASE};

const RULER_HEIGHT: f32 = 24.0;
const TRACK_HEIGHT: f32 = 56.0;
const TRACK_GAP: f32 = 2.0;
/// Width of the trim handles' *hit zone*. Deliberately generous — this is
/// the one gesture in the whole widget that demands pixel precision from
/// the user (everything else tolerates a sloppy click), so it gets the most
/// forgiving target. The drawn accent stripe inside it (`HANDLE_ACCENT_WIDTH`)
/// stays visually thin so clips don't look like they're missing a chunk.
const HANDLE_WIDTH: f32 = 10.0;
const HANDLE_ACCENT_WIDTH: f32 = 4.0;
/// Snap range in *pixels*. Converted to ticks at the current zoom on every
/// use, because snapping has to feel like a constant on-screen distance — a
/// fixed tick tolerance would grab from across the room when zoomed out and be
/// unreachable when zoomed in.
const SNAP_PX: f64 = 8.0;

fn snap_tolerance_ticks(state: &EditorState) -> i64 {
    (SNAP_PX * state.ticks_per_px) as i64
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState, waveforms: &mut WaveformCache) {
    ui.horizontal(|ui| {
        if icon_toggle(ui, Icon::Select, "Select", state.tool == Tool::Select).on_hover_text("V").clicked() {
            state.tool = Tool::Select;
        }
        if icon_toggle(ui, Icon::Razor, "Razor", state.tool == Tool::Razor).on_hover_text("C").clicked() {
            state.tool = Tool::Razor;
        }
        ui.separator();
        if icon_button(ui, Icon::ZoomIn, "").on_hover_text("Zoom in").clicked() {
            state.ticks_per_px = (state.ticks_per_px / 1.5).max(TIMEBASE as f64 / 4000.0);
        }
        if icon_button(ui, Icon::ZoomOut, "").on_hover_text("Zoom out").clicked() {
            state.ticks_per_px = (state.ticks_per_px * 1.5).min(TIMEBASE as f64 * 2.0);
        }
        ui.separator();
        if icon_toggle(ui, Icon::Snap, "", state.snapping).on_hover_text("Snap (S)").clicked() {
            state.snapping = !state.snapping;
        }
        if icon_button(ui, Icon::MarkIn, "")
            .on_hover_text("set the in point at the playhead (I)")
            .clicked()
        {
            state.mark_in();
        }
        if icon_button(ui, Icon::MarkOut, "").on_hover_text("set the out point at the playhead (O)").clicked() {
            state.mark_out();
        }
        if state.in_point.is_some() || state.out_point.is_some() {
            if ui.button("Clear marks").clicked() {
                state.clear_marks();
            }
            if let Some((i, o)) = state.marked_range() {
                ui.weak(format!(
                    "in/out: {:.2}s-{:.2}s",
                    i as f64 / TIMEBASE as f64,
                    o as f64 / TIMEBASE as f64
                ));
            }
        }
        ui.separator();
        ui.label(format!(
            "playhead: {:.2}s / {:.2}s",
            state.playhead as f64 / TIMEBASE as f64,
            state.sequence_duration_ticks() as f64 / TIMEBASE as f64
        ));
        if state.selected_clips.len() > 1 {
            ui.weak(format!("{} clips selected", state.selected_clips.len()));
        }
        if state.shuttle_rate != 0.0 && state.shuttle_rate != 1.0 {
            ui.colored_label(
                Color32::from_rgb(150, 220, 255),
                format!("shuttle {:+.0}x (silent)", state.shuttle_rate),
            );
        }
        if !state.playback_health.is_empty() {
            ui.colored_label(Color32::from_rgb(255, 170, 60), &state.playback_health);
        }
        if !state.status.is_empty() {
            ui.colored_label(Color32::LIGHT_RED, &state.status);
        }
    });

    let width = ui.available_width().max(200.0);
    let origin_x = ui.cursor().left();

    let tick_to_x = |tick: i64, state: &EditorState| -> f32 {
        origin_x + ((tick - state.scroll_ticks) as f64 / state.ticks_per_px) as f32
    };
    let x_to_tick = |x: f32, state: &EditorState| -> i64 {
        state.scroll_ticks + ((x - origin_x) as f64 * state.ticks_per_px) as i64
    };

    // --- Ruler ---
    let (ruler_rect, ruler_resp) =
        ui.allocate_exact_size(Vec2::new(width, RULER_HEIGHT), Sense::click_and_drag());
    draw_ruler(ui, ruler_rect, state, &tick_to_x);
    if ruler_resp.dragged() || ruler_resp.clicked() {
        if let Some(pos) = ruler_resp.interact_pointer_pos() {
            state.playhead = x_to_tick(pos.x, state).max(0);
            state.playing = false;
        }
    }

    // --- Track lanes, video tracks on top (last-in-vec = topmost layer),
    // audio tracks below, matching the Premiere convention even though the
    // underlying model just stores one bottom-to-top Vec per spec.
    let tracks = state.sequence().tracks.clone();
    let mut video_ids: Vec<TrackId> = tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .map(|t| t.id)
        .collect();
    video_ids.reverse();
    let audio_ids: Vec<TrackId> = tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio)
        .map(|t| t.id)
        .collect();
    let display_order: Vec<TrackId> = video_ids.into_iter().chain(audio_ids).collect();

    for track_id in display_order {
        let track = tracks.iter().find(|t| t.id == track_id).unwrap();
        let (lane_rect, lane_resp) =
            ui.allocate_exact_size(Vec2::new(width, TRACK_HEIGHT), Sense::click());
        ui.painter()
            .rect_filled(lane_rect, 0.0, lane_color(track.kind));
        ui.painter().text(
            lane_rect.left_top() + Vec2::new(4.0, 2.0),
            egui::Align2::LEFT_TOP,
            &track.name,
            egui::FontId::proportional(11.0),
            Color32::from_gray(200),
        );

        if state.tool == Tool::Razor && lane_resp.clicked() {
            if let Some(pos) = lane_resp.interact_pointer_pos() {
                let at = x_to_tick(pos.x, state).max(0);
                let new_id = timeline::ClipInstanceId(state.next_id());
                state.apply_op(
                    "razor",
                    EditOp::Razor {
                        track: track_id,
                        at: TimeTick(at),
                        new_clip_id: new_id,
                    },
                );
            }
        }

        for clip in &track.clips {
            let x0 = tick_to_x(clip.timeline_in.0, state);
            let x1 = tick_to_x(clip.timeline_out.0, state);
            if x1 < lane_rect.left() || x0 > lane_rect.right() {
                continue; // off-screen, skip drawing/interaction
            }
            let clip_rect = Rect::from_min_max(
                Pos2::new(x0.max(lane_rect.left()), lane_rect.top() + TRACK_GAP),
                Pos2::new(x1.min(lane_rect.right()), lane_rect.bottom() - TRACK_GAP),
            );
            draw_and_interact_clip(ui, state, track_id, clip, clip_rect, &x_to_tick);
            // Waveform after the clip body so it sits on top of the fill,
            // and only on audio tracks — Premiere shows waveforms there and
            // thumbnails on video tracks, and a waveform under a video clip
            // would just be the same linked audio drawn twice.
            if track.kind == TrackKind::Audio {
                draw_waveform(ui, state, waveforms, clip, clip_rect);
            }
        }

        // Transitions last, so they read as sitting *over* the cut they join
        // rather than being hidden behind either clip.
        for tr in &track.transitions {
            let (start, end) = tr.region();
            let (x0, x1) = (tick_to_x(start.0, state), tick_to_x(end.0, state));
            if x1 < lane_rect.left() || x0 > lane_rect.right() {
                continue;
            }
            let rect = Rect::from_min_max(
                Pos2::new(x0.max(lane_rect.left()), lane_rect.top() + TRACK_GAP),
                Pos2::new(x1.min(lane_rect.right()), lane_rect.bottom() - TRACK_GAP),
            );
            ui.painter().rect_filled(
                rect,
                2.0,
                Color32::from_rgba_unmultiplied(240, 240, 255, 70),
            );
            ui.painter()
                .rect_stroke(rect, 2.0, Stroke::new(1.0f32, Color32::from_gray(230)));
            // Two crossing diagonals: the conventional NLE glyph for a
            // transition, and it reads at any width without needing a label.
            ui.painter().line_segment(
                [rect.left_top(), rect.right_bottom()],
                Stroke::new(1.0f32, Color32::from_gray(240)),
            );
            ui.painter().line_segment(
                [rect.left_bottom(), rect.right_top()],
                Stroke::new(1.0f32, Color32::from_gray(240)),
            );
            if rect.width() > 46.0 {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    match tr.kind {
                        timeline::TransitionKind::CrossDissolve => "dissolve",
                        timeline::TransitionKind::DipToBlack => "dip",
                        timeline::TransitionKind::Wipe => "wipe",
                        timeline::TransitionKind::Slide => "slide",
                    },
                    egui::FontId::proportional(9.0),
                    Color32::WHITE,
                );
            }
        }
    }

    // --- Playhead line, drawn over every lane ---
    let full_height = RULER_HEIGHT + TRACK_HEIGHT * (state.sequence().tracks.len().max(1)) as f32;
    let px = tick_to_x(state.playhead, state);
    let top = ruler_rect.top();
    ui.painter().line_segment(
        [Pos2::new(px, top), Pos2::new(px, top + full_height)],
        Stroke::new(2.0f32, Color32::from_rgb(255, 80, 80)),
    );
}

fn draw_and_interact_clip(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    track_id: TrackId,
    clip: &ClipInstance,
    clip_rect: Rect,
    x_to_tick: &impl Fn(f32, &EditorState) -> i64,
) {
    let selected = state.is_selected(clip.id);
    let base_color = if clip.effects.is_empty() {
        Color32::from_rgb(70, 110, 160)
    } else {
        Color32::from_rgb(90, 130, 90)
    };
    let fill = if selected {
        Color32::from_rgb(230, 190, 90)
    } else {
        base_color
    };

    let can_have_handles = clip_rect.width() > HANDLE_WIDTH * 3.0 && state.tool == Tool::Select;
    let (body_rect, left_handle, right_handle) = if can_have_handles {
        (
            Rect::from_min_max(
                clip_rect.min + Vec2::new(HANDLE_WIDTH, 0.0),
                clip_rect.max - Vec2::new(HANDLE_WIDTH, 0.0),
            ),
            Some(Rect::from_min_max(
                clip_rect.min,
                Pos2::new(clip_rect.min.x + HANDLE_WIDTH, clip_rect.max.y),
            )),
            Some(Rect::from_min_max(
                Pos2::new(clip_rect.max.x - HANDLE_WIDTH, clip_rect.min.y),
                clip_rect.max,
            )),
        )
    } else {
        (clip_rect, None, None)
    };

    ui.painter().rect_filled(clip_rect, 3.0, fill);
    ui.painter()
        .rect_stroke(clip_rect, 3.0, Stroke::new(1.0f32, Color32::from_gray(20)));
    ui.painter().text(
        clip_rect.left_top() + Vec2::new(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        clip_label(state, clip),
        egui::FontId::proportional(10.0),
        Color32::WHITE,
    );

    if state.tool == Tool::Razor {
        return; // razor clicks are handled at the lane level
    }

    let body_id = egui::Id::new(("clip_body", clip.id.0));
    let body_resp = ui
        .interact(body_rect, body_id, Sense::click_and_drag())
        .on_hover_text("Right-click for transition options (cross dissolve, wipe, slide...)");
    if body_resp.clicked() {
        // Ctrl/Cmd-click extends the selection; a plain click replaces it.
        // Matches every NLE, and matters because the delete/copy shortcuts now
        // act on the whole selection.
        if ui.input(|i| i.modifiers.command) {
            state.toggle_in_selection(clip.id);
        } else {
            state.select_only(clip.id);
        }
    }
    if body_resp.drag_started() {
        // Dragging an already-selected clip keeps the selection (so a
        // multi-clip selection survives a drag); dragging an unselected one
        // selects just it.
        if !state.is_selected(clip.id) {
            state.select_only(clip.id);
        }
        if let Some(pos) = body_resp.interact_pointer_pos() {
            let grab_offset_ticks = x_to_tick(pos.x, state) - clip.timeline_in.0;
            state.drag = Some(Drag::MoveClip {
                clip: clip.id,
                grab_offset_ticks,
            });
            state.begin_drag_edit("move clip");
        }
    }
    if body_resp.dragged() {
        if let (
            Some(Drag::MoveClip {
                clip: c,
                grab_offset_ticks,
            }),
            Some(pos),
        ) = (&state.drag, body_resp.interact_pointer_pos())
        {
            if *c == clip.id {
                let raw_in = x_to_tick(pos.x, state) - grab_offset_ticks;
                // Snap whichever edge lands closer to a snap point, then move
                // by the same delta — snapping the head only would make it
                // impossible to butt a clip's *tail* against the next clip,
                // which is half of what snapping is for.
                let duration = clip.timeline_out.0 - clip.timeline_in.0;
                let tol = snap_tolerance_ticks(state);
                let snapped_in = state.snap_tick(raw_in, Some(clip.id), tol);
                let snapped_out = state.snap_tick(raw_in + duration, Some(clip.id), tol);
                let new_in = if (snapped_in - raw_in).abs() <= (snapped_out - (raw_in + duration)).abs()
                {
                    snapped_in
                } else {
                    snapped_out - duration
                };
                state.move_clip(clip.id, track_id, new_in);
            }
        }
    }
    if body_resp.drag_stopped() {
        if matches!(&state.drag, Some(Drag::MoveClip { clip: c, .. }) if *c == clip.id) {
            state.end_drag_edit();
        }
    }

    // Transitions live on cuts, so they're offered from the clip whose edge
    // forms the cut — no separate hit target to find, and no ambiguity about
    // which of two adjacent cuts was meant.
    body_resp.context_menu(|ui| {
        let track_has = |state: &EditorState, at: timeline::TimeTick| {
            state
                .sequence()
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .is_some_and(|t| t.transitions.iter().any(|tr| tr.at == at))
        };
        for (label, at) in [
            ("start of this clip", clip.timeline_in),
            ("end of this clip", clip.timeline_out),
        ] {
            ui.menu_button(format!("Transition at {label}"), |ui| {
                // Driven by `TransitionKind::ALL` rather than a hand-written
                // button per kind, so adding a variant to the model can't
                // leave it implemented but unofferable.
                for kind in timeline::TransitionKind::ALL {
                    if ui.button(kind.label()).clicked() {
                        state.add_transition(
                            track_id,
                            at,
                            kind,
                            timeline::TimeTick(EditorState::DEFAULT_TRANSITION_TICKS),
                        );
                        ui.close_menu();
                    }
                }
                if track_has(state, at) && ui.button("Remove").clicked() {
                    state.remove_transition(track_id, at);
                    ui.close_menu();
                }
            });
        }
    });

    if let Some(left_handle) = left_handle {
        trim_handle(ui, state, left_handle, clip.id, true, x_to_tick);
    }
    if let Some(right_handle) = right_handle {
        trim_handle(ui, state, right_handle, clip.id, false, x_to_tick);
    }
}

/// One trim handle: a wide hit zone (`HANDLE_WIDTH`) that's filled edge to
/// edge (not just a thin accent floating inside it) so the whole clickable
/// area is visibly clickable, not just a sliver of it — and brightened on
/// hover/drag so the grab zone is unambiguous before and during the
/// gesture. `is_left` selects which edge of `TrimRipple` this handle drives
/// (`new_in` vs `new_out`) — both are always-safe ripple trims, so this
/// never needs its own overlap handling the way a body move does.
fn trim_handle(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    handle_rect: Rect,
    clip_id: timeline::ClipInstanceId,
    is_left: bool,
    x_to_tick: &impl Fn(f32, &EditorState) -> i64,
) {
    let id = egui::Id::new((if is_left { "trim_in" } else { "trim_out" }, clip_id.0));
    let resp = ui.interact(handle_rect, id, Sense::drag());

    let dragging_this = match &state.drag {
        Some(Drag::TrimIn { clip: c, .. }) => is_left && *c == clip_id,
        Some(Drag::TrimOut { clip: c }) => !is_left && *c == clip_id,
        _ => false,
    };
    let active = resp.hovered() || dragging_this;
    if active {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    // Fill the full hit zone at low alpha so it always reads as "part of
    // the clip, but distinct" — then brighten it on hover/drag so the
    // active grab zone is unmistakable. The accent stripe at the outer
    // edge stays at full opacity in both states as a permanent "this is an
    // edge" cue, independent of hover.
    let fill_alpha = if active { 130 } else { 55 };
    ui.painter()
        .rect_filled(handle_rect, 0.0, Color32::from_black_alpha(fill_alpha));
    let accent_color = if active {
        Color32::from_gray(240)
    } else {
        Color32::from_gray(160)
    };
    let accent_x = if is_left {
        handle_rect.left()
    } else {
        handle_rect.right() - HANDLE_ACCENT_WIDTH
    };
    let accent_rect = Rect::from_min_max(
        Pos2::new(accent_x, handle_rect.top()),
        Pos2::new(accent_x + HANDLE_ACCENT_WIDTH, handle_rect.bottom()),
    );
    ui.painter().rect_filled(accent_rect, 0.0, accent_color);

    if resp.drag_started() {
        state.drag = Some(if is_left {
            Drag::TrimIn {
                clip: clip_id,
                base: state.project().clone(),
            }
        } else {
            Drag::TrimOut { clip: clip_id }
        });
        state.begin_drag_edit(if is_left { "trim in" } else { "trim out" });
    }
    if resp.dragged() && dragging_this {
        if let Some(pos) = resp.interact_pointer_pos() {
            let tick = x_to_tick(pos.x, state).max(0);
            if is_left {
                let Some(Drag::TrimIn { base, .. }) = state.drag.clone() else {
                    unreachable!()
                };
                state.apply_op_coalescing_from(
                    &base,
                    EditOp::TrimRipple {
                        clip: clip_id,
                        new_in: Some(TimeTick(tick)),
                        new_out: None,
                    },
                );
            } else {
                state.apply_op_coalescing(EditOp::TrimRipple {
                    clip: clip_id,
                    new_in: None,
                    new_out: Some(TimeTick(tick)),
                });
            }
        }
    }
    if resp.drag_stopped() && dragging_this {
        state.end_drag_edit();
    }
}

/// Paints the clip's audio envelope inside `clip_rect`, one vertical line per
/// pixel column.
///
/// The mapping that makes this correct under editing: each column's x is
/// converted to a *source* sample offset, via the clip's `source_in` plus how
/// far into the clip that column sits, run through `SpeedCurve::source_delta`.
/// That's the same conversion the render graph and audio mixer use, so a
/// trimmed, moved, or retimed clip shows the part of the waveform it actually
/// plays rather than always starting from the head of the file.
fn draw_waveform(
    ui: &mut egui::Ui,
    state: &EditorState,
    waveforms: &mut WaveformCache,
    clip: &ClipInstance,
    clip_rect: Rect,
) {
    let ClipSource::Media(asset_id) = clip.source else {
        return; // nested sequences have no single source file to read peaks from
    };
    let Some(path) = state.asset_paths.get(&asset_id).cloned() else { return };
    let Some(peaks) = waveforms.peaks(asset_id, &path) else {
        // Still generating (or unavailable). Draw nothing rather than a
        // placeholder — a fake flat line would read as "this clip is silent",
        // which is worse than an empty clip body that fills in a moment later.
        return;
    };
    if peaks.sample_rate == 0 {
        return;
    }

    let mid_y = clip_rect.center().y;
    let half_height = (clip_rect.height() / 2.0 - 2.0).max(1.0);
    let color = Color32::from_rgba_unmultiplied(20, 40, 30, 200);
    let ticks_per_px = state.ticks_per_px;

    // Walk whole pixels so adjacent columns tile exactly with no gaps.
    let x_start = clip_rect.left().floor() as i32;
    let x_end = clip_rect.right().ceil() as i32;
    let clip_left = clip_rect.left() as f64;
    let sample_of = |offset_into_clip_ticks: i64| -> i64 {
        let source_ticks = clip.source_in.0 + clip.speed.source_delta(offset_into_clip_ticks);
        (source_ticks as i128 * peaks.sample_rate as i128 / TIMEBASE as i128) as i64
    };

    for x in x_start..x_end {
        // Ticks into the clip covered by this one pixel column.
        let from_ticks = ((x as f64 - clip_left) * ticks_per_px) as i64;
        let to_ticks = (((x + 1) as f64 - clip_left) * ticks_per_px) as i64;
        let Some((min, max)) = column_range(&peaks, sample_of(from_ticks), sample_of(to_ticks))
        else {
            continue;
        };
        let top = mid_y - max.clamp(-1.0, 1.0) * half_height;
        let bottom = mid_y - min.clamp(-1.0, 1.0) * half_height;
        // Always at least a hair tall, so near-silence still shows a centre
        // line instead of vanishing.
        let (top, bottom) = if (bottom - top).abs() < 1.0 {
            (mid_y - 0.5, mid_y + 0.5)
        } else {
            (top, bottom)
        };
        let px = x as f32 + 0.5;
        ui.painter()
            .line_segment([Pos2::new(px, top), Pos2::new(px, bottom)], Stroke::new(1.0f32, color));
    }
}

fn clip_label(state: &EditorState, clip: &ClipInstance) -> String {
    match &clip.source {
        // The text itself, so a timeline of titles is readable at a glance
        // rather than a row of identical "title" labels. First line only, and
        // trimmed — a long title would otherwise be drawn past the clip's
        // right edge and over its neighbour.
        timeline::ClipSource::Title(spec) => {
            let first = spec.text.lines().next().unwrap_or("").trim();
            match first.char_indices().nth(24) {
                Some((cut, _)) => format!("{}…", &first[..cut]),
                None if first.is_empty() => "(empty title)".into(),
                None => first.to_string(),
            }
        }
        &timeline::ClipSource::Media(asset_id) => state
            .project()
            .assets
            .iter()
            .find(|a| a.id == asset_id)
            .and_then(|a| std::path::Path::new(&a.original_absolute_path).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "media".into()),
        &timeline::ClipSource::NestedSequence(id) => format!("sequence {}", id.0),
    }
}

fn lane_color(kind: TrackKind) -> Color32 {
    match kind {
        TrackKind::Video => Color32::from_gray(38),
        TrackKind::Audio => Color32::from_gray(30),
    }
}

fn draw_ruler(
    ui: &mut egui::Ui,
    rect: Rect,
    state: &EditorState,
    tick_to_x: &impl Fn(i64, &EditorState) -> f32,
) {
    ui.painter().rect_filled(rect, 0.0, Color32::from_gray(50));

    // In/out marks, drawn before the ticks so the numbers stay readable on top
    // of the shaded range.
    if let Some((i, o)) = state.marked_range() {
        let (xi, xo) = (tick_to_x(i, state), tick_to_x(o, state));
        ui.painter().rect_filled(
            Rect::from_min_max(
                Pos2::new(xi.max(rect.left()), rect.top()),
                Pos2::new(xo.min(rect.right()), rect.bottom()),
            ),
            0.0,
            Color32::from_rgba_unmultiplied(120, 180, 255, 40),
        );
    }
    for (mark, is_in) in [(state.in_point, true), (state.out_point, false)] {
        let Some(m) = mark else { continue };
        let x = tick_to_x(m, state);
        if x < rect.left() || x > rect.right() {
            continue;
        }
        ui.painter().line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(2.0f32, Color32::from_rgb(120, 200, 255)),
        );
        // A small flag pointing into the marked range, so in and out are
        // distinguishable at a glance rather than being two identical lines.
        let dir = if is_in { 6.0 } else { -6.0 };
        ui.painter().add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(x, rect.top()),
                Pos2::new(x + dir, rect.top()),
                Pos2::new(x, rect.top() + 6.0),
            ],
            Color32::from_rgb(120, 200, 255),
            Stroke::NONE,
        ));
    }

    let seconds_per_100px = state.ticks_per_px * 100.0 / TIMEBASE as f64;
    let step_secs = *[1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0]
        .iter()
        .find(|&&s| s >= seconds_per_100px)
        .unwrap_or(&1800.0);
    let step_ticks = (step_secs * TIMEBASE as f64) as i64;
    if step_ticks <= 0 {
        return;
    }
    let first = (state.scroll_ticks / step_ticks) * step_ticks;
    let mut t = first;
    while tick_to_x(t, state) < rect.right() {
        let x = tick_to_x(t, state);
        if x >= rect.left() {
            ui.painter().line_segment(
                [
                    Pos2::new(x, rect.bottom() - 6.0),
                    Pos2::new(x, rect.bottom()),
                ],
                Stroke::new(1.0f32, Color32::from_gray(150)),
            );
            let secs = t as f64 / TIMEBASE as f64;
            ui.painter().text(
                Pos2::new(x + 2.0, rect.top()),
                egui::Align2::LEFT_TOP,
                format!("{:02}:{:02}", (secs / 60.0) as u32, (secs % 60.0) as u32),
                egui::FontId::proportional(10.0),
                Color32::from_gray(200),
            );
        }
        t += step_ticks;
    }
}
