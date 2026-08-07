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
}

impl UndoStack {
    pub const DEFAULT_MAX_HISTORY: usize = 100;

    pub fn new(initial: Arc<Project>, max_history: usize) -> Self {
        UndoStack { current: initial, history: Vec::new(), redo: Vec::new(), max_history, coalescing: None }
    }

    pub fn current(&self) -> &Arc<Project> {
        &self.current
    }

    /// One discrete undo step. Use `begin_coalescing`/`end_coalescing`
    /// instead for interactive drags — spec 4.3: "dragging a clip is one
    /// undo step, not 400."
    pub fn push(&mut self, label: impl Into<String>, after: Arc<Project>) {
        assert!(self.coalescing.is_none(), "cannot push while a coalescing group is open");
        let before = std::mem::replace(&mut self.current, after.clone());
        self.history.push(CommandEntry { label: label.into(), before, after });
        self.redo.clear();
        if self.history.len() > self.max_history {
            self.history.remove(0);
        }
    }

    /// Marks the start of a coalescing group (e.g. mouse-down on a drag).
    /// Every intermediate state change until `end_coalescing` collapses into
    /// a single undo entry.
    pub fn begin_coalescing(&mut self, label: impl Into<String>) {
        assert!(self.coalescing.is_none(), "coalescing group already open");
        self.coalescing = Some(CoalesceGroup { label: label.into(), before: self.current.clone() });
    }

    /// Updates the live project during an open coalescing group (e.g. mouse-move
    /// during a drag) without creating an undo entry yet.
    pub fn update_coalescing(&mut self, after: Arc<Project>) {
        assert!(self.coalescing.is_some(), "no coalescing group open");
        self.current = after;
    }

    /// Commits the coalescing group as a single undo entry (e.g. mouse-up).
    pub fn end_coalescing(&mut self) {
        let group = self.coalescing.take().expect("no coalescing group open");
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
        Arc::new(Project { sequences: vec![], assets: vec![] })
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
}
