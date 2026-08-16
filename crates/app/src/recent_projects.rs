//! The home screen's "recent projects" list: a small, self-healing MRU file
//! at `%LOCALAPPDATA%\nle-engine\recent_projects.txt` — one absolute path
//! per line, most-recently-opened first.
//!
//! Deliberately plain text, not JSON: the only thing worth remembering per
//! entry is the path itself (name, size, and last-modified all come from
//! the filesystem at display time, so they can't go stale independently of
//! the file they describe), which makes a serde dependency pure overhead
//! for this one file.

use std::path::{Path, PathBuf};

/// Long enough to feel like real history, short enough that the home screen
/// doesn't turn into an unscrollable list of every project ever opened.
const MAX_ENTRIES: usize = 15;

fn config_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    base.join("nle-engine")
}

pub fn default_list_path() -> PathBuf {
    config_dir().join("recent_projects.txt")
}

/// Reads `list_path`, keeping only entries whose file still exists on disk
/// (a project that was deleted or lived on a now-unplugged drive shouldn't
/// linger on the home screen as a dead link) and writing the pruned list
/// back so the self-healing doesn't have to happen again next launch.
pub fn load(list_path: &Path) -> Vec<PathBuf> {
    let Ok(contents) = std::fs::read_to_string(list_path) else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    let mut any_pruned = false;
    for line in contents.lines() {
        let path = PathBuf::from(line);
        if path.is_file() {
            kept.push(path);
        } else if !line.is_empty() {
            any_pruned = true;
        }
    }
    if any_pruned {
        let _ = write_all(list_path, &kept);
    }
    kept
}

/// Moves `path` to the front of the list at `list_path` (inserting it if
/// it's new), dedupes, caps at `MAX_ENTRIES`, and persists the result.
/// Silently does nothing if the config directory can't be created or
/// written — a failure here should never block opening or saving a
/// project, since the recent-projects list is a convenience, not data the
/// user's actual work depends on.
pub fn record_opened(list_path: &Path, path: &Path) {
    let mut entries = load(list_path);
    entries.retain(|p| p != path);
    entries.insert(0, path.to_path_buf());
    entries.truncate(MAX_ENTRIES);
    let _ = write_all(list_path, &entries);
}

fn write_all(list_path: &Path, entries: &[PathBuf]) -> std::io::Result<()> {
    if let Some(dir) = list_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = entries.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
    std::fs::write(list_path, body)
}

/// Bundles the list file's location with its in-memory contents, so
/// call sites (`main.rs`) only need to thread one value through instead of
/// a path and a `Vec` separately.
pub struct RecentProjects {
    list_path: PathBuf,
    entries: Vec<PathBuf>,
}

impl RecentProjects {
    pub fn load_default() -> Self {
        let list_path = default_list_path();
        let entries = load(&list_path);
        RecentProjects { list_path, entries }
    }

    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    pub fn record(&mut self, path: &Path) {
        record_opened(&self.list_path, path);
        self.entries.retain(|p| p != path);
        self.entries.insert(0, path.to_path_buf());
        self.entries.truncate(MAX_ENTRIES);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn a_missing_list_file_loads_as_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let list_path = dir.path().join("recent_projects.txt");
        assert_eq!(load(&list_path), Vec::<PathBuf>::new());
    }

    #[test]
    fn recording_a_project_makes_it_load_back() {
        let dir = tempfile::tempdir().unwrap();
        let list_path = dir.path().join("recent_projects.txt");
        let project = touch(dir.path(), "a.nleproj");

        record_opened(&list_path, &project);

        assert_eq!(load(&list_path), vec![project]);
    }

    #[test]
    fn the_most_recently_opened_project_sorts_first() {
        let dir = tempfile::tempdir().unwrap();
        let list_path = dir.path().join("recent_projects.txt");
        let a = touch(dir.path(), "a.nleproj");
        let b = touch(dir.path(), "b.nleproj");

        record_opened(&list_path, &a);
        record_opened(&list_path, &b);

        assert_eq!(load(&list_path), vec![b, a]);
    }

    #[test]
    fn reopening_an_already_listed_project_moves_it_to_the_front_without_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let list_path = dir.path().join("recent_projects.txt");
        let a = touch(dir.path(), "a.nleproj");
        let b = touch(dir.path(), "b.nleproj");

        record_opened(&list_path, &a);
        record_opened(&list_path, &b);
        record_opened(&list_path, &a);

        assert_eq!(load(&list_path), vec![a, b]);
    }

    #[test]
    fn the_list_is_capped_so_it_cannot_grow_without_bound() {
        let dir = tempfile::tempdir().unwrap();
        let list_path = dir.path().join("recent_projects.txt");
        let mut expected_newest_first = Vec::new();
        for i in 0..(MAX_ENTRIES + 5) {
            let p = touch(dir.path(), &format!("p{i}.nleproj"));
            record_opened(&list_path, &p);
            expected_newest_first.insert(0, p);
        }
        let loaded = load(&list_path);
        assert_eq!(loaded.len(), MAX_ENTRIES, "list must not grow past the cap");
        assert_eq!(loaded, &expected_newest_first[..MAX_ENTRIES], "must keep the most recent entries, not the oldest");
    }

    #[test]
    fn a_project_file_that_no_longer_exists_is_silently_dropped_on_load() {
        // The most important behavior here: opening a project, then
        // deleting or moving that file outside the app, must not leave a
        // dead entry the user can click and get an error from.
        let dir = tempfile::tempdir().unwrap();
        let list_path = dir.path().join("recent_projects.txt");
        let a = touch(dir.path(), "a.nleproj");
        let b = touch(dir.path(), "b.nleproj");
        record_opened(&list_path, &a);
        record_opened(&list_path, &b);

        std::fs::remove_file(&a).unwrap();

        assert_eq!(load(&list_path), vec![b]);
    }

    #[test]
    fn pruning_a_dead_entry_persists_so_it_stays_gone_next_load() {
        let dir = tempfile::tempdir().unwrap();
        let list_path = dir.path().join("recent_projects.txt");
        let a = touch(dir.path(), "a.nleproj");
        record_opened(&list_path, &a);
        std::fs::remove_file(&a).unwrap();

        load(&list_path); // triggers the self-heal
        let contents = std::fs::read_to_string(&list_path).unwrap();

        assert!(!contents.contains("a.nleproj"), "a pruned entry must not resurrect on the next load");
    }

    #[test]
    fn recording_creates_the_config_directory_if_it_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let list_path = dir.path().join("nested").join("recent_projects.txt");
        let a = touch(dir.path(), "a.nleproj");

        record_opened(&list_path, &a);

        assert_eq!(load(&list_path), vec![a]);
    }
}
