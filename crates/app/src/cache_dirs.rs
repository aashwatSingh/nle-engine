//! Scratch directories for derived media — proxies and background-removal
//! mattes.
//!
//! Both are per-process (`nle-proxies-<pid>`, `nle-mattes-<pid>`) so two
//! editors running at once can't overwrite each other's files. Nothing used
//! to delete them, so every run left its directory behind permanently:
//! mattes are lossless QTRLE at roughly 4 MB per second of footage, which
//! reaches gigabytes over a few sessions.
//!
//! Two halves clean that up. `remove` runs on a clean exit and handles the
//! ordinary case. `sweep` runs at startup and handles the rest — a crash or a
//! force-kill never reaches the exit path, so without it those directories
//! would still accumulate forever.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The directory-name prefixes this module owns. Anything else in the temp
/// directory belongs to somebody else and is never touched.
const PREFIXES: [&str; 2] = ["nle-proxies-", "nle-mattes-"];

/// How stale another run's directory must be before `sweep` deletes it.
///
/// The age check is what makes the sweep safe against a *live* second editor:
/// its directory is minutes old, not days, so it is never a candidate. A
/// day is far longer than any plausible gap between a crash and the next
/// launch, and costs only the disk that one crashed session left behind.
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// `<temp>/<prefix><our pid>`, created if it doesn't exist.
pub(crate) fn create(prefix: &str) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("{prefix}{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Deletes a scratch directory and everything in it. Best effort: a file
/// still open elsewhere is not worth failing an exit over, and the sweep
/// will get it next launch.
pub(crate) fn remove(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Deletes scratch directories left behind by earlier runs, returning how
/// many it removed.
///
/// Skips this process's own directories outright, and anything younger than
/// `stale_after`, so a concurrently running editor keeps its cache. Errors
/// are ignored per directory: one unreadable entry shouldn't stop the rest.
pub(crate) fn sweep(root: &Path, stale_after: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    let ours = std::process::id().to_string();
    let mut removed = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if !PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        if name.ends_with(&ours) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|age| age >= stale_after).unwrap_or(false))
            .unwrap_or(false);
        if !stale {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Production entry point: sweep the OS temp directory at startup.
pub(crate) fn sweep_temp() -> usize {
    sweep(&std::env::temp_dir(), STALE_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with_file(root: &Path, name: &str) -> PathBuf {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("payload.bin"), b"x").unwrap();
        d
    }

    #[test]
    fn sweep_removes_scratch_directories_from_earlier_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let proxies = dir_with_file(tmp.path(), "nle-proxies-111");
        let mattes = dir_with_file(tmp.path(), "nle-mattes-222");

        // Zero staleness so the age check can't mask what's being selected.
        assert_eq!(sweep(tmp.path(), Duration::ZERO), 2);
        assert!(!proxies.exists());
        assert!(!mattes.exists());
    }

    /// The one that matters: this deletes directories recursively, so it must
    /// never reach anything it doesn't own.
    #[test]
    fn sweep_never_touches_directories_it_does_not_own() {
        let tmp = tempfile::tempdir().unwrap();
        let someone_else = dir_with_file(tmp.path(), "important-other-app");
        let near_miss = dir_with_file(tmp.path(), "nle-something-else-333");
        let loose_file = tmp.path().join("nle-proxies-not-a-dir.txt");
        std::fs::write(&loose_file, b"x").unwrap();

        assert_eq!(sweep(tmp.path(), Duration::ZERO), 0);
        assert!(someone_else.join("payload.bin").exists(), "unrelated directory must survive");
        assert!(near_miss.join("payload.bin").exists(), "a different nle- prefix is not ours");
        assert!(loose_file.exists(), "a file, not a directory, must be left alone");
    }

    /// A second editor running right now must keep its cache, or its proxies
    /// and mattes vanish from under it mid-session.
    #[test]
    fn sweep_leaves_this_processs_own_directories_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let ours = dir_with_file(tmp.path(), &format!("nle-mattes-{}", std::process::id()));

        assert_eq!(sweep(tmp.path(), Duration::ZERO), 0);
        assert!(ours.join("payload.bin").exists(), "our own cache must survive a sweep");
    }

    #[test]
    fn remove_deletes_the_directory_and_its_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dir_with_file(tmp.path(), "nle-mattes-999");

        remove(&d);

        assert!(!d.exists());
    }
}
