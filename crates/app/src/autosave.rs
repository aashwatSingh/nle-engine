//! Autosave and crash recovery.
//!
//! ## What this protects against
//!
//! Losing an editing session to a crash, a power cut, or closing the window by
//! accident. The editor is a long-session application with an undo stack that
//! only lives in memory, so "I hadn't saved yet" is a total loss without this.
//!
//! ## Why it writes a *separate* recovery file
//!
//! Autosave never touches the user's own project file. Overwriting it would
//! make autosave destructive: an accidental edit would be committed to disk
//! without the user ever choosing to save, and "close without saving" would
//! stop meaning anything. The recovery file sits beside the project (or in the
//! temp directory for a never-saved project) and is deleted on a clean exit or
//! an explicit save.
//!
//! ## Why presence of the file *is* the crash signal
//!
//! There's no separate "did we crash" flag to get out of sync. A recovery file
//! that still exists at startup means the last session wrote one and never got
//! to clean it up — which is exactly the condition worth offering to restore.
//! A clean exit removes it, so no false positives.
//!
//! Undo history is not carried into the recovery file. `ProjectDocument`
//! supports persisting it, but a recovery write happens every few seconds and
//! serialising the whole history each time would make autosave cost scale with
//! how long you'd been working — the opposite of what a background safety net
//! should do.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often to write, when there is something to write. Long enough that the
/// write cost is irrelevant, short enough that a crash costs seconds of work.
const INTERVAL: Duration = Duration::from_secs(20);

pub struct Autosave {
    last_write: Instant,
    /// Undo-stack depth at the last write, used as a cheap "has anything
    /// changed" check. Comparing project contents would mean a deep compare of
    /// the whole project every tick; the undo depth changes on exactly the
    /// events that matter.
    last_saved_revision: usize,
    /// The file currently on disk, if any — tracked so it can be removed on a
    /// clean exit even after the project path changes.
    written: Option<PathBuf>,
    pub last_error: Option<String>,
}

impl Default for Autosave {
    fn default() -> Self {
        Autosave {
            // Not `Instant::now()`: starting "due" would write an empty project
            // the moment the editor opens, creating a recovery file that looks
            // like a crashed session on the next launch.
            last_write: Instant::now(),
            last_saved_revision: 0,
            written: None,
            last_error: None,
        }
    }
}

impl Autosave {
    /// Recovery path for a project: beside it, prefixed so it sorts next to the
    /// original and is obviously not the user's file. Never-saved projects go to
    /// the temp directory, since there's no project folder to sit beside yet.
    pub fn recovery_path_for(project_path: Option<&Path>) -> PathBuf {
        match project_path {
            Some(p) => {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "project.nleproj".into());
                p.with_file_name(format!(".recover-{name}"))
            }
            None => std::env::temp_dir().join("nle-untitled-recovery.nleproj"),
        }
    }

    /// True when enough time has passed *and* the project has changed since the
    /// last write. Both conditions matter: a timer alone would rewrite an
    /// unchanged project forever, keeping a stale recovery file alive and making
    /// every launch offer to restore work identical to what's already saved.
    pub fn is_due(&self, revision: usize) -> bool {
        revision != self.last_saved_revision && self.last_write.elapsed() >= INTERVAL
    }

    /// Records a successful write of `path` at revision `revision`.
    pub fn mark_written(&mut self, path: PathBuf, revision: usize) {
        self.last_write = Instant::now();
        self.last_saved_revision = revision;
        self.written = Some(path);
        self.last_error = None;
    }

    pub fn mark_failed(&mut self, error: String) {
        // Still reset the timer: a failing write that retried every frame would
        // turn a full disk into a stuttering editor.
        self.last_write = Instant::now();
        self.last_error = Some(error);
    }

    /// Called after an explicit save, and on clean exit: the user's own file now
    /// holds this work, so a recovery file would only produce a spurious
    /// "restore?" prompt next launch.
    pub fn discard(&mut self, revision: usize) {
        if let Some(path) = self.written.take() {
            let _ = std::fs::remove_file(path);
        }
        self.last_saved_revision = revision;
    }

    /// The recovery file left by a previous session, if there is one.
    pub fn find_recovery(project_path: Option<&Path>) -> Option<PathBuf> {
        let path = Self::recovery_path_for(project_path);
        path.exists().then_some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recovery_file_sits_beside_its_project_and_is_clearly_not_the_project() {
        let p = PathBuf::from("C:/work/film.nleproj");
        let r = Autosave::recovery_path_for(Some(&p));
        assert_eq!(r.parent(), p.parent(), "should sit beside the project");
        assert_ne!(r, p, "must never be the project file itself");
        assert!(
            r.file_name().unwrap().to_string_lossy().starts_with(".recover-"),
            "should be obviously a recovery file, got {r:?}"
        );
    }

    #[test]
    fn an_unsaved_project_still_gets_a_recovery_path() {
        // The case autosave matters most in: a session that has never been
        // saved has the most to lose.
        let r = Autosave::recovery_path_for(None);
        assert!(r.is_absolute());
        assert!(r.to_string_lossy().contains("recovery"));
    }

    #[test]
    fn nothing_is_due_before_the_interval_even_after_an_edit() {
        let a = Autosave::default();
        assert!(!a.is_due(5), "a fresh editor must not write immediately");
    }

    #[test]
    fn an_unchanged_project_is_never_due_however_long_it_sits() {
        // A timer-only check would rewrite forever, keeping a stale recovery
        // file alive and making the next launch offer to restore work that is
        // already safely saved.
        let mut a = Autosave::default();
        a.last_write = Instant::now() - Duration::from_secs(3600);
        a.last_saved_revision = 7;
        assert!(!a.is_due(7), "no change means no write, no matter the elapsed time");
        assert!(a.is_due(8), "a change after the interval is due");
    }

    #[test]
    fn discard_removes_the_file_so_the_next_launch_does_not_offer_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".recover-x.nleproj");
        std::fs::write(&path, b"x").unwrap();

        let mut a = Autosave::default();
        a.mark_written(path.clone(), 3);
        assert!(path.exists());

        a.discard(3);
        assert!(!path.exists(), "a clean save/exit must remove the recovery file");
        // And it's not due again at the same revision.
        a.last_write = Instant::now() - Duration::from_secs(3600);
        assert!(!a.is_due(3));
    }

    #[test]
    fn a_failed_write_backs_off_instead_of_retrying_every_frame() {
        let mut a = Autosave::default();
        a.last_write = Instant::now() - Duration::from_secs(3600);
        assert!(a.is_due(1));

        a.mark_failed("disk full".into());
        assert!(!a.is_due(1), "a failure must reset the timer, not spin");
        assert_eq!(a.last_error.as_deref(), Some("disk full"));
    }

    #[test]
    fn find_recovery_reports_only_a_file_that_actually_exists() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p.nleproj");
        assert!(Autosave::find_recovery(Some(&project)).is_none());

        let recovery = Autosave::recovery_path_for(Some(&project));
        std::fs::write(&recovery, b"x").unwrap();
        assert_eq!(Autosave::find_recovery(Some(&project)), Some(recovery));
    }
}
