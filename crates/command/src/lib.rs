//! Command/undo system, per spec 4.3. Every action is a
//! `(before, after)` pair of full `Project` snapshots plus a label — not a
//! mutable object graph with a separate undo log, which the spec explicitly
//! calls out as an anti-pattern to avoid (section 10). Because `timeline`
//! versions are meant to be cheap to hold onto (`Arc<Project>`), storing a
//! bounded history of them is affordable.
//!
//! Decision (docs/decisions-log.md): undo history IS persisted from v1.0,
//! bounded to `max_history` entries (default 100) to cap project file size —
//! this was left `[DECIDE]`-optional in the original spec but deciding it
//! now avoids a schema migration later.

use std::sync::Arc;
use timeline::Project;

pub struct CommandEntry {
    pub label: String,
    pub before: Arc<Project>,
    pub after: Arc<Project>,
}

struct CoalesceGroup {
    label: String,
    before: Arc<Project>,
}

pub struct UndoStack {
    current: Arc<Project>,
    history: Vec<CommandEntry>,
    redo: Vec<CommandEntry>,
    max_history: usize,
    coalescing: Option<CoalesceGroup>,
    revision: u64,
}

impl UndoStack {
    pub const DEFAULT_MAX_HISTORY: usize = 100;

    pub fn new(initial: Arc<Project>, max_history: usize) -> Self {
        UndoStack {
            current: initial,
            history: Vec::new(),
            redo: Vec::new(),
            max_history,
            coalescing: None,
            revision: 0,
        }
    }

    pub fn current(&self) -> &Arc<Project> {
        &self.current
    }

    /// The undo history, oldest first — for persistence.
    ///
    /// Redo is deliberately not exposed for saving. Redo entries describe states
    /// *ahead* of `current`, i.e. work the user undid before saving; restoring
    /// them would let a reopened project "redo" its way to a state the saved
    /// file never contained, which is a confusing thing for a file to be able to
    /// do. Premiere and Resolve both drop redo on reopen for the same reason.
    pub fn history(&self) -> &[CommandEntry] {
        &self.history
    }

    pub fn max_history(&self) -> usize {
        self.max_history
    }

    /// Rebuilds a stack from a persisted `current` plus its history.
    ///
    /// `history` is oldest-first and is truncated to the newest `max_history`
    /// entries, so loading a file written by a build with a larger cap can't
    /// exceed this one's bound.
    ///
    /// The caller is trusted for consistency (that `history.last().after`
    /// matches `current`); a mismatched pair would make the first undo jump to
    /// an unrelated state rather than fail, so `EditorState::open_from` checks
    /// it rather than assuming a file on disk is well-formed.
    pub fn restore(current: Arc<Project>, history: Vec<CommandEntry>, max_history: usize) -> Self {
        let mut history = history;
        if history.len() > max_history {
            let drop = history.len() - max_history;
            history.drain(0..drop);
        }
        UndoStack {
            current,
            history,
            // Empty on purpose — see `history`'s comment.
            redo: Vec::new(),
            max_history,
            coalescing: None,
            revision: 0,
        }
    }

    /// Monotonic counter bumped by every change to `current`.
    ///
    /// Exists so callers (autosave) can answer "has anything changed since I
    /// last looked?" in constant time. Deliberately **not** `history.len()`:
    /// history is bounded, so it stops growing once full, and undo pops it —
    /// both of which make it repeat values that autosave would read as "no
    /// change" and skip a write that was needed. Monotonic means an undo counts
    /// as a change too, which is correct: undoing back to a previously-saved
    /// state is still a state the user might want recovered.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// One discrete undo step. Use `begin_coalescing`/`end_coalescing`
    /// instead for interactive drags — spec 4.3: "dragging a clip is one
    /// undo step, not 400."
    pub fn push(&mut self, label: impl Into<String>, after: Arc<Project>) {
        // An edit arriving mid-gesture commits the gesture first rather than
        // panicking. This used to assert, and the assert was reachable from
        // ordinary use: a keystroke while the mouse is still down, or a
        // background analysis landing on a frame during a drag — `poll` runs
        // every frame whatever the pointer is doing. Crashing the editor and
        // taking the unsaved project with it is a far worse answer to "these
        // two overlapped" than deciding what the overlap means.
        //
        // The invariant the assert protected still holds. It existed so a
        // drag couldn't be recorded as 400 separate steps; closing the group
        // first yields exactly two — the gesture as one step, then this edit
        // as its own — which is also what someone pressing Ctrl+Z would
        // expect to walk back through.
        self.close_open_group();
        let before = std::mem::replace(&mut self.current, after.clone());
        self.revision += 1;
        self.history.push(CommandEntry { label: label.into(), before, after });
        self.redo.clear();
        if self.history.len() > self.max_history {
            self.history.remove(0);
        }
    }

    /// Pushes a new entry, or — when the most recent entry carries the same
    /// `label` — amends that entry in place, so a run of related edits stays
    /// one undo step.
    ///
    /// The discrete-event counterpart to `begin_coalescing`/`end_coalescing`.
    /// A drag knows when it begins and ends (mouse down, mouse up), so an
    /// explicitly-scoped group fits it. Typing into a text field has no such
    /// bracket: there is no event that reliably means "the user has stopped
    /// typing", and an open coalescing group that nothing closes makes Ctrl+Z
    /// silently do nothing until focus happens to change. Amending keeps the
    /// history consistent after *every* keystroke — so undo always works —
    /// while still collapsing the run into one step.
    ///
    /// `label` is the identity of the thing being edited, not just a
    /// description: callers must make it distinguish separate objects (e.g.
    /// by including a clip id), or edits to two different titles would fold
    /// into a single entry that undoes both.
    pub fn push_or_amend(&mut self, label: impl Into<String>, after: Arc<Project>) {
        // Same reasoning as `push`.
        self.close_open_group();
        let label = label.into();
        let before = std::mem::replace(&mut self.current, after.clone());
        self.revision += 1;
        match self.history.last_mut() {
            // Amending keeps the run's original `before`, so one undo steps
            // back past the whole run rather than to its second-to-last state.
            Some(last) if last.label == label => last.after = after,
            _ => self.history.push(CommandEntry { label, before, after }),
        }
        // A redo stack surviving a fresh edit would let the user redo their
        // way into a state this edit never came from — same reasoning as
        // `push`, and just as true when amending.
        self.redo.clear();
        if self.history.len() > self.max_history {
            self.history.remove(0);
        }
    }

    /// Marks the start of a coalescing group (e.g. mouse-down on a drag).
    /// Every intermediate state change until `end_coalescing` collapses into
    /// a single undo entry.
    pub fn begin_coalescing(&mut self, label: impl Into<String>) {
        // A group still open here means the previous gesture's end was never
        // observed — a mouse-up lost to a focus change, say. Committing what
        // it accumulated is better than either panicking or silently folding
        // two unrelated gestures into one undo entry.
        self.close_open_group();
        self.coalescing = Some(CoalesceGroup { label: label.into(), before: self.current.clone() });
    }

    /// Updates the live project during an open coalescing group (e.g. mouse-move
    /// during a drag) without creating an undo entry yet.
    pub fn update_coalescing(&mut self, after: Arc<Project>) {
        // No group open means one was committed underneath this gesture (see
        // `push`). The update is still a real change the user made, so it
        // becomes its own entry rather than being dropped.
        if self.coalescing.is_none() {
            self.push("edit", after);
            return;
        }
        self.current = after;
        self.revision += 1;
    }

    /// Commits the coalescing group as a single undo entry (e.g. mouse-up).
    /// Commits an open group, if there is one. Idempotent: closing a
    /// gesture that was already committed underneath it is a no-op, not a
    /// panic, because whoever opened it has no way to know that happened.
    pub fn end_coalescing(&mut self) {
        self.close_open_group();
    }

    /// Whether a gesture is currently accumulating into one undo entry.
    /// The single source of truth — callers that track it separately drift.
    pub fn is_coalescing(&self) -> bool {
        self.coalescing.is_some()
    }

    fn close_open_group(&mut self) {
        let Some(group) = self.coalescing.take() else { return };
        self.history.push(CommandEntry { label: group.label, before: group.before, after: self.current.clone() });
        self.redo.clear();
        if self.history.len() > self.max_history {
            self.history.remove(0);
        }
    }

    pub fn undo(&mut self) -> bool {
        match self.history.pop() {
            Some(entry) => {
                self.current = entry.before.clone();
                self.revision += 1;
                self.redo.push(entry);
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.redo.pop() {
            Some(entry) => {
                self.current = entry.after.clone();
                self.revision += 1;
                self.history.push(entry);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_project() -> Arc<Project> {
        Arc::new(Project { sequences: vec![], assets: vec![], bins: vec![] })
    }

    /// A distinct project version, so pushes are real state changes.
    fn versioned(n: u128) -> Arc<Project> {
        let mut p = Project { sequences: vec![], assets: vec![], bins: vec![] };
        p.assets.push(fake_asset(n));
        Arc::new(p)
    }

    #[test]
    fn amending_a_run_of_same_label_edits_leaves_one_undo_step() {
        let v0 = empty_project();
        let mut stack = UndoStack::new(v0.clone(), 10);

        for n in 1..=5 {
            stack.push_or_amend("edit title 7", versioned(n));
        }

        assert_eq!(stack.history().len(), 1, "five edits to one thing is one step");
        assert!(stack.undo());
        assert_eq!(
            stack.current(),
            &v0,
            "one undo must step back past the whole run, not to its second-to-last state"
        );
    }

    #[test]
    fn a_different_label_starts_its_own_undo_step() {
        // The guard that keeps two titles from folding into one entry that
        // undoes both.
        let mut stack = UndoStack::new(empty_project(), 10);

        stack.push_or_amend("edit title 1", versioned(1));
        stack.push_or_amend("edit title 1", versioned(2));
        stack.push_or_amend("edit title 2", versioned(3));
        stack.push_or_amend("edit title 2", versioned(4));

        assert_eq!(stack.history().len(), 2);
        stack.undo();
        assert_eq!(stack.current(), &versioned(2), "undoing title 2 leaves title 1's edit intact");
    }

    #[test]
    fn an_amended_run_is_undoable_after_every_single_edit() {
        // The whole reason this exists rather than an open coalescing group:
        // Ctrl+Z must work mid-typing, with no "done editing" event needed.
        let v0 = empty_project();
        let mut stack = UndoStack::new(v0.clone(), 10);
        stack.push_or_amend("edit title 7", versioned(1));

        assert!(stack.undo(), "undo must work immediately after the first edit");
        assert_eq!(stack.current(), &v0);
    }

    #[test]
    fn amending_clears_redo_like_any_other_edit() {
        let mut stack = UndoStack::new(empty_project(), 10);
        stack.push("something", versioned(1));
        stack.undo();
        assert!(stack.redo(), "precondition: there is something to redo");
        stack.undo();

        stack.push_or_amend("edit title 7", versioned(2));
        assert!(!stack.redo(), "a fresh edit must not leave a redo into an abandoned branch");
    }

    #[test]
    fn amending_bumps_the_revision_every_time_even_though_history_does_not_grow() {
        // Autosave watches `revision`. If amending didn't bump it, every
        // keystroke after the first in a title would look like "no change"
        // and go unsaved.
        let mut stack = UndoStack::new(empty_project(), 10);
        let start = stack.revision();
        for n in 1..=4 {
            stack.push_or_amend("edit title 7", versioned(n));
        }
        assert_eq!(stack.history().len(), 1);
        assert_eq!(stack.revision(), start + 4, "each edit is a real change to save");
    }

    #[test]
    fn undo_redo_round_trips_through_versions() {
        let v0 = empty_project();
        let mut stack = UndoStack::new(v0.clone(), 10);

        let mut v1_inner = (*v0).clone();
        v1_inner.assets.push(fake_asset(1));
        let v1 = Arc::new(v1_inner);
        stack.push("add asset 1", v1.clone());

        assert_eq!(stack.current().assets.len(), 1);
        assert!(stack.undo());
        assert_eq!(**stack.current(), *v0);
        assert!(stack.redo());
        assert_eq!(**stack.current(), *v1);
        assert!(!stack.redo());
    }

    #[test]
    fn coalescing_collapses_to_one_undo_step() {
        let v0 = empty_project();
        let mut stack = UndoStack::new(v0.clone(), 10);

        stack.begin_coalescing("drag clip");
        for i in 1..=5 {
            let mut p = (*v0).clone();
            p.assets.push(fake_asset(i));
            stack.update_coalescing(Arc::new(p));
        }
        stack.end_coalescing();

        assert_eq!(stack.current().assets.len(), 1);
        assert!(stack.undo());
        assert_eq!(**stack.current(), *v0);
        assert!(!stack.undo());
    }

    #[test]
    fn history_is_bounded() {
        let v0 = empty_project();
        let mut stack = UndoStack::new(v0.clone(), 3);
        for i in 1..=5u128 {
            let mut p = (**stack.current()).clone();
            p.assets.push(fake_asset(i));
            let next = Arc::new(p);
            stack.push(format!("add {i}"), next);
        }
        let mut undo_count = 0;
        while stack.undo() {
            undo_count += 1;
        }
        assert_eq!(undo_count, 3, "history should be capped at max_history entries");
    }

    fn fake_asset(id: u128) -> media::MediaAsset {
        media::MediaAsset {
            id: media::MediaAssetId(id),
            original_absolute_path: format!("C:/fake/{id}.mov"),
            content_hash: [0u8; 32],
            container_format: "mov".into(),
            video: None,
            audio: None,
            duration_ticks: 0,
        }
    }

    #[test]
    fn revision_advances_on_every_change_including_undo() {
        // Autosave uses this to decide whether anything needs writing, so a
        // repeated value means a skipped write. `history.len()` was the obvious
        // candidate and is wrong twice over: it stops growing once the bounded
        // history is full, and undo pops it back to a value already seen.
        let mut stack = UndoStack::new(empty_project(), 4);
        let start = stack.revision();

        stack.push("a", versioned(1));
        let after_push = stack.revision();
        assert!(after_push > start);

        stack.undo();
        assert!(stack.revision() > after_push, "undo is a change too");
        let after_undo = stack.revision();
        stack.redo();
        assert!(stack.revision() > after_undo, "and so is redo");
    }

    #[test]
    fn revision_keeps_advancing_after_the_bounded_history_is_full() {
        // The specific case `history.len()` gets wrong: once capped, its length
        // is constant, so a change-detector built on it goes permanently blind.
        let mut stack = UndoStack::new(empty_project(), 2);
        for i in 1..=6 {
            stack.push("x", versioned(i));
        }
        let full = stack.revision();
        stack.push("y", versioned(99));
        assert!(stack.revision() > full, "revision must not stall once history is capped");
    }

    #[test]
    fn a_coalesced_drag_advances_revision_as_it_moves() {
        // A drag is one undo entry but many states; autosave should be able to
        // notice that work happened even mid-gesture.
        let mut stack = UndoStack::new(empty_project(), 8);
        let start = stack.revision();
        stack.begin_coalescing("drag");
        stack.update_coalescing(versioned(1));
        stack.update_coalescing(versioned(2));
        assert!(stack.revision() > start);
        stack.end_coalescing();
    }
}
