//! Parameter animation: keyframes, their interpolation and tangents.

use super::*;

impl EditorState {
    /// Runs `f` against one parameter's track and pushes the result.
    fn with_param_mut(
        &mut self,
        label: &str,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        f: impl FnOnce(&mut timeline::ParamTrack),
    ) {
        if let Some(project) = self.with_clip_mut(clip_id, |clip| {
            if let Some(effect) = clip.effects.iter_mut().find(|e| e.id == effect_id) {
                if let Some(track) = effect.params.get_mut(param_name) {
                    f(track);
                }
            }
        }) {
            self.undo.push(label, std::sync::Arc::new(project));
        }
    }


    /// Turns animation on or off for a parameter, Premiere's stopwatch.
    ///
    /// Both directions deliberately preserve the value visible at `local`:
    /// enabling seeds a keyframe from the current constant, and disabling
    /// collapses to whatever the curve evaluated to right there. Otherwise
    /// toggling the stopwatch would make the picture jump — losing work in one
    /// direction and silently changing the frame in the other.
    pub fn toggle_param_animation(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        local: TimeTick,
    ) {
        self.with_param_mut("toggle animation", clip_id, effect_id, param_name, |track| {
            if track.is_animated() {
                let held = track.evaluate_at(local);
                track.keyframes.clear();
                track.default = held;
            } else {
                let seed = track.default;
                track.upsert_keyframe(local, seed, timeline::InterpolationMode::Linear);
            }
        });
    }


    /// Adds a keyframe at `local` holding the parameter's current value there,
    /// or removes the one already at that exact time.
    pub fn toggle_keyframe(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        local: TimeTick,
    ) {
        self.with_param_mut("toggle keyframe", clip_id, effect_id, param_name, |track| {
            if track.keyframe_index_at(local).is_some() {
                track.remove_keyframe_at(local);
            } else {
                let value = track.evaluate_at(local);
                track.upsert_keyframe(local, value, timeline::InterpolationMode::Linear);
            }
        });
    }


    pub fn set_keyframe_interpolation(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        local: TimeTick,
        mode: timeline::InterpolationMode,
    ) {
        self.with_param_mut("set interpolation", clip_id, effect_id, param_name, |track| {
            track.set_interpolation_at(local, mode);
        });
    }


    /// Sets a keyframe's explicit bezier tangent handles, in (time-ticks-delta,
    /// value-delta) space as `Keyframe::tangents` stores them.
    pub fn set_keyframe_tangents(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        local: TimeTick,
        tangents: ((f64, f64), (f64, f64)),
    ) {
        self.with_param_mut("edit tangent", clip_id, effect_id, param_name, |track| {
            track.set_tangents_at(local, tangents);
        });
    }


    pub fn move_keyframe(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        from: TimeTick,
        to: TimeTick,
    ) {
        self.with_param_mut("move keyframe", clip_id, effect_id, param_name, |track| {
            track.move_keyframe(from, to);
        });
    }


    /// Writes a parameter value the way an NLE does: to the keyframe at the
    /// playhead when the parameter is animated, to the constant otherwise.
    ///
    /// This is the whole point of the keyframe UI — without the animated
    /// branch, editing a slider on an animated parameter would change
    /// `default`, which `evaluate_at` ignores entirely whenever any keyframe
    /// exists. The edit would appear to do nothing.
    pub fn set_param_value_at(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        value: ParamValue,
        local: Option<TimeTick>,
        coalescing: bool,
    ) {
        let project = self.with_clip_mut(clip_id, |clip| {
            if let Some(effect) = clip.effects.iter_mut().find(|e| e.id == effect_id) {
                if let Some(track) = effect.params.get_mut(param_name) {
                    match (track.is_animated(), local) {
                        (true, Some(local)) => track.upsert_keyframe(
                            local,
                            value,
                            timeline::InterpolationMode::Linear,
                        ),
                        // Animated but the playhead is off the clip: there's no
                        // keyframe time to write to, so leave the curve alone
                        // rather than silently editing an arbitrary one.
                        (true, None) => {}
                        (false, _) => track.default = value,
                    }
                }
            }
        });
        let Some(project) = project else { return };
        if coalescing {
            self.undo.update_coalescing(std::sync::Arc::new(project));
        } else {
            self.undo.push("edit effect", std::sync::Arc::new(project));
        }
    }

}
