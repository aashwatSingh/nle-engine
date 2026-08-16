//! The screen shown before a project is open: a way to start something new
//! or pick up recent work, instead of always landing straight in a blank
//! untitled editor with no memory of anything you'd opened before.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub enum HomeAction {
    None,
    NewProject,
    OpenPath(PathBuf),
    OpenDialog,
}

/// Coarse "how long ago", matching what a recent-files list actually needs
/// (never precision finer than a minute) without pulling in a date/time
/// crate for calendar math this doesn't require.
fn relative_time(modified: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(modified).map(|d| d.as_secs()).unwrap_or(0);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        let m = secs / 60;
        format!("{m} minute{} ago", if m == 1 { "" } else { "s" })
    } else if secs < 86_400 {
        let h = secs / 3600;
        format!("{h} hour{} ago", if h == 1 { "" } else { "s" })
    } else if secs < 86_400 * 30 {
        let d = secs / 86_400;
        format!("{d} day{} ago", if d == 1 { "" } else { "s" })
    } else {
        let mo = secs / (86_400 * 30);
        format!("{mo} month{} ago", if mo == 1 { "" } else { "s" })
    }
}

fn project_name(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "untitled".into())
}

pub fn show(ui: &mut egui::Ui, recent: &[PathBuf]) -> HomeAction {
    let mut action = HomeAction::None;

    ui.add_space(48.0);
    ui.vertical_centered(|ui| {
        ui.heading(egui::RichText::new("nle-engine").size(28.0));
        ui.weak("Pick up where you left off, or start something new.");
    });
    ui.add_space(28.0);

    ui.vertical_centered(|ui| {
        ui.horizontal(|ui| {
            ui.add_space(ui.available_width() / 2.0 - 130.0);
            if ui.add_sized([120.0, 40.0], egui::Button::new("New Project")).clicked() {
                action = HomeAction::NewProject;
            }
            if ui.add_sized([120.0, 40.0], egui::Button::new("Open...")).clicked() {
                action = HomeAction::OpenDialog;
            }
        });
    });

    ui.add_space(32.0);
    ui.separator();
    ui.add_space(8.0);
    ui.label(egui::RichText::new("Recent Projects").strong());

    if recent.is_empty() {
        ui.add_space(8.0);
        ui.weak("Nothing here yet — projects you open or save will show up on this screen.");
        return action;
    }

    let now = SystemTime::now();
    egui::ScrollArea::vertical().show(ui, |ui| {
        for path in recent {
            let modified = std::fs::metadata(path).and_then(|m| m.modified()).unwrap_or(now);
            let row_id = ui.make_persistent_id(("recent_project_row", path));
            let resp = ui.push_id(row_id, |ui| {
                ui.horizontal(|ui| {
                    let name_resp = ui.selectable_label(false, egui::RichText::new(project_name(path)).strong());
                    ui.weak(relative_time(modified, now));
                    ui.weak(path.to_string_lossy().into_owned());
                    name_resp
                })
                .inner
            });
            if resp.inner.clicked() {
                action = HomeAction::OpenPath(path.clone());
            }
        }
    });

    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn very_recent_reads_as_just_now() {
        let now = SystemTime::now();
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - Duration::from_secs(30), now), "just now");
    }

    #[test]
    fn minutes_hours_days_and_months_are_each_worded_correctly() {
        let now = SystemTime::now();
        assert_eq!(relative_time(now - Duration::from_secs(60 * 5), now), "5 minutes ago");
        assert_eq!(relative_time(now - Duration::from_secs(60), now), "1 minute ago");
        assert_eq!(relative_time(now - Duration::from_secs(3600 * 3), now), "3 hours ago");
        assert_eq!(relative_time(now - Duration::from_secs(3600), now), "1 hour ago");
        assert_eq!(relative_time(now - Duration::from_secs(86_400 * 2), now), "2 days ago");
        assert_eq!(relative_time(now - Duration::from_secs(86_400 * 45), now), "1 month ago");
    }

    #[test]
    fn a_project_named_with_dots_still_yields_a_clean_stem() {
        // `file_stem` only strips the *last* extension — worth pinning,
        // since a project called "final.v2.nleproj" should read as
        // "final.v2", not silently lose the ".v2".
        assert_eq!(project_name(Path::new("C:/work/final.v2.nleproj")), "final.v2");
    }

    #[test]
    fn a_path_with_no_file_name_falls_back_to_untitled() {
        assert_eq!(project_name(Path::new("/")), "untitled");
    }
}
