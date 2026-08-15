//! The Project panel: Premiere's media database view. Bins (folders) you can
//! nest, metadata columns you can sort by, a search filter, and sequences
//! listed alongside footage.
//!
//! Two behaviours copied deliberately from Premiere because they're what make
//! the panel usable rather than just present:
//!
//! - **Search flattens the tree.** Typing in the filter shows every match
//!   wherever it lives, with its bin named alongside — you don't have to know
//!   which folder you filed something in to find it again. (Premiere shows
//!   the same flattened result list.)
//! - **Deleting a bin keeps its contents**, promoting them to the parent. A
//!   folder delete that took the footage with it would be a data-loss trap.
//!
//! Moves are done through a context menu rather than drag-and-drop. Premiere
//! uses drag, and that's nicer, but a reliable tree drag-drop in egui is a
//! chunk of work on its own (drop targets on rows *and* on the whitespace
//! between them, auto-expand on hover, reorder-vs-reparent disambiguation).
//! The context menu does the same job correctly today; drag is a follow-up.

use crate::state::EditorState;
use timeline::{BinId, BinItem, TIMEBASE};

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mov", "m4v", "mkv", "webm", "avi", "wmv", "flv", "ts", "mts", "m2ts", "3gp",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Kind,
    Duration,
}

/// Purely visual panel state — which folders are open, what's typed in the
/// search box, how rows are ordered. Kept out of `EditorState` because none
/// of it is part of the document or worth an undo step.
pub struct ProjectPanelState {
    pub expanded: std::collections::HashSet<BinId>,
    pub search: String,
    pub sort: SortKey,
    pub sort_ascending: bool,
    /// Bin currently being renamed, plus the in-progress text.
    pub renaming: Option<(BinId, String)>,
    /// Whether this rename session has already grabbed keyboard focus.
    /// See the comment at the `request_focus` call for why re-requesting
    /// every frame wedges the panel.
    pub rename_focus_sent: bool,
}

impl Default for ProjectPanelState {
    fn default() -> Self {
        ProjectPanelState {
            expanded: std::collections::HashSet::new(),
            search: String::new(),
            sort: SortKey::Name,
            sort_ascending: true,
            renaming: None,
            rename_focus_sent: false,
        }
    }
}

/// One row's worth of derived metadata, so sorting and rendering agree on
/// exactly the same values.
struct ItemInfo {
    item: BinItem,
    name: String,
    kind: &'static str,
    duration_ticks: i64,
    detail: String,
}

fn item_info(state: &EditorState, item: BinItem) -> ItemInfo {
    let name = state.item_name(item);
    match item {
        BinItem::Asset(id) => {
            let asset = state.project().assets.iter().find(|a| a.id == id);
            let kind = match asset.map(|a| (a.video.is_some(), a.audio.is_some())) {
                Some((true, true)) => "A/V",
                Some((true, false)) => "Video",
                Some((false, true)) => "Audio",
                _ => "—",
            };
            let detail = asset
                .and_then(|a| a.video.as_ref())
                .map(|v| format!("{}x{}", v.width, v.height))
                .unwrap_or_else(|| "—".into());
            ItemInfo {
                item,
                name,
                kind,
                duration_ticks: asset.map(|a| a.duration_ticks).unwrap_or(0),
                detail,
            }
        }
        BinItem::Sequence(id) => {
            let seq = state.project().sequences.iter().find(|s| s.id == id);
            let detail = seq
                .map(|s| format!("{}x{}", s.settings.width, s.settings.height))
                .unwrap_or_else(|| "—".into());
            ItemInfo {
                item,
                name,
                kind: "Sequence",
                duration_ticks: seq.map(|s| s.duration().0).unwrap_or(0),
                detail,
            }
        }
    }
}

fn format_duration(ticks: i64) -> String {
    if ticks <= 0 {
        return "—".into();
    }
    let secs = ticks as f64 / TIMEBASE as f64;
    let m = (secs / 60.0).floor() as u32;
    let s = secs - m as f64 * 60.0;
    format!("{m}:{s:04.1}")
}

/// The row ordering, as a comparator so both the tree view (which sorts
/// `ItemInfo`s) and the search view (which sorts them paired with a bin name)
/// order rows identically instead of each reimplementing it.
fn compare_items(panel: &ProjectPanelState, a: &ItemInfo, b: &ItemInfo) -> std::cmp::Ordering {
    let ord = match panel.sort {
        SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        SortKey::Kind => a
            .kind
            .cmp(b.kind)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
        SortKey::Duration => a.duration_ticks.cmp(&b.duration_ticks),
    };
    if panel.sort_ascending {
        ord
    } else {
        ord.reverse()
    }
}

pub fn show(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    panel: &mut ProjectPanelState,
    proxies: &mut crate::proxy_jobs::ProxyJobs,
) {
    ui.horizontal(|ui| {
        ui.heading("Project");
        if ui.button("Import...").clicked() {
            if let Some(paths) = rfd::FileDialog::new()
                .set_title("Import media")
                .add_filter("Video files", VIDEO_EXTENSIONS)
                .add_filter("All files", &["*"])
                .pick_files()
            {
                state.import_assets(paths);
            }
        }
        if ui.button("New Bin").clicked() {
            // New bins land inside the selected item's bin, so creating one
            // while working in a folder keeps you there rather than jumping
            // back to the root.
            let parent = state.selected_item.and_then(|i| state.project().bin_of(i));
            let id = state.create_bin("New Bin", parent);
            panel.expanded.insert(id);
            panel.renaming = Some((id, "New Bin".into()));
            panel.rename_focus_sent = false;
        }
    });

    // Proxy controls. Building is explicit and off by default: a proxy is
    // minutes of transcoding per file, so the editor must never decide to spend
    // that on its own.
    ui.horizontal(|ui| {
        ui.checkbox(&mut proxies.enabled, "Use proxies")
            .on_hover_text("play and scrub from small all-intra copies where they exist");
        if ui
            .button("Build proxies")
            .on_hover_text("transcode every imported clip to a 960px all-intra copy")
            .clicked()
        {
            let assets: Vec<(media::MediaAssetId, std::path::PathBuf)> = state
                .asset_paths
                .iter()
                .map(|(id, p)| (*id, p.clone()))
                .collect();
            let mut started = 0;
            for (id, path) in assets {
                if proxies.request(id, path) {
                    started += 1;
                }
            }
            state.status = if started == 0 {
                "nothing to build — proxies already exist or are running".into()
            } else {
                format!("building {started} proxy/proxies in the background")
            };
        }
    });
    if proxies.is_busy() {
        ui.weak(format!(
            "{} building, {} ready",
            proxies.in_flight_count(),
            proxies.ready_count()
        ));
        // Keep repainting so the state markers update as jobs land, instead of
        // waiting for the next mouse move.
        ui.ctx().request_repaint();
    } else if proxies.ready_count() > 0 {
        ui.weak(format!("{} proxies ready", proxies.ready_count()));
    }
    if let Some(err) = &proxies.last_error {
        ui.colored_label(egui::Color32::LIGHT_RED, err);
    }

    ui.horizontal(|ui| {
        ui.label("Search:");
        ui.add(egui::TextEdit::singleline(&mut panel.search).desired_width(120.0));
        if !panel.search.is_empty() && ui.small_button("clear").clicked() {
            panel.search.clear();
        }
    });

    // Clickable column headers, Premiere-style: click to sort, click the
    // active one again to reverse.
    ui.horizontal(|ui| {
        let mut header = |ui: &mut egui::Ui, label: &str, key: SortKey, width: f32| {
            let active = panel.sort == key;
            let text = if active {
                format!("{label} {}", if panel.sort_ascending { "^" } else { "v" })
            } else {
                label.to_string()
            };
            let resp = ui.add_sized([width, 18.0], egui::SelectableLabel::new(active, text));
            if resp.clicked() {
                if active {
                    panel.sort_ascending = !panel.sort_ascending;
                } else {
                    panel.sort = key;
                    panel.sort_ascending = true;
                }
            }
        };
        header(ui, "Name", SortKey::Name, 130.0);
        header(ui, "Type", SortKey::Kind, 60.0);
        header(ui, "Dur", SortKey::Duration, 55.0);
    });
    ui.separator();

    egui::ScrollArea::vertical()
        .max_height(320.0)
        .show(ui, |ui| {
            if panel.search.trim().is_empty() {
                draw_bin_contents(ui, state, panel, proxies, None, 0);
            } else {
                draw_search_results(ui, state, panel, proxies);
            }
        });

    ui.separator();
    let can_add = matches!(state.selected_item, Some(BinItem::Asset(_)));
    if ui
        .add_enabled(can_add, egui::Button::new("Add to Timeline"))
        .clicked()
    {
        if let Some(BinItem::Asset(id)) = state.selected_item {
            state.append_asset_to_timeline(id);
        }
    }
}

/// Recursive tree render: child bins first (folders above files, as every
/// file browser does), then this bin's items.
#[allow(clippy::too_many_arguments)]
fn draw_bin_contents(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    panel: &mut ProjectPanelState,
    proxies: &mut crate::proxy_jobs::ProxyJobs,
    parent: Option<BinId>,
    depth: usize,
) {
    // Collected up front because drawing takes `&mut state`, which can't be
    // held across a borrow of the project.
    let child_bins: Vec<(BinId, String)> = state
        .project()
        .child_bins(parent)
        .iter()
        .map(|b| (b.id, b.name.clone()))
        .collect();

    for (bin_id, bin_name) in child_bins {
        draw_bin_row(ui, state, panel, bin_id, &bin_name, depth);
        if panel.expanded.contains(&bin_id) {
            draw_bin_contents(ui, state, panel, proxies, Some(bin_id), depth + 1);
        }
    }

    let items: Vec<BinItem> = match parent {
        None => state.project().root_items(),
        Some(id) => state
            .project()
            .bins
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.items.clone())
            .unwrap_or_default(),
    };
    let mut infos: Vec<ItemInfo> = items.into_iter().map(|i| item_info(state, i)).collect();
    infos.sort_by(|a, b| compare_items(panel, a, b));
    for info in &infos {
        draw_item_row(ui, state, proxies, info, depth, None);
    }
}

fn draw_bin_row(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    panel: &mut ProjectPanelState,
    bin_id: BinId,
    bin_name: &str,
    depth: usize,
) {
    // Stable id per bin — see `draw_item_row` for why layout-derived ids
    // break the context menu.
    ui.push_id(egui::Id::new(("project_bin", bin_id.0)), |ui| {
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 12.0);
            let expanded = panel.expanded.contains(&bin_id);
            if ui.small_button(if expanded { "-" } else { "+" }).clicked() {
                if expanded {
                    panel.expanded.remove(&bin_id);
                } else {
                    panel.expanded.insert(bin_id);
                }
            }

            // Inline rename, committed on Enter or focus loss — the same gesture
            // as renaming a folder in a file browser.
            if let Some((renaming_id, buffer)) =
                panel.renaming.as_mut().filter(|(id, _)| *id == bin_id)
            {
                let resp = ui.add(egui::TextEdit::singleline(buffer).desired_width(120.0));
                // Focus is requested only on the first frame of the rename, not
                // every frame. Re-requesting each frame keeps yanking focus back
                // here, which locks the whole panel: clicks elsewhere and every
                // other text field stop responding because this one steals focus
                // again before the next frame is drawn.
                if !resp.has_focus() && !panel.rename_focus_sent {
                    resp.request_focus();
                    panel.rename_focus_sent = true;
                }
                let committed = resp.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter));
                if committed {
                    let (id, name) = (*renaming_id, buffer.clone());
                    state.rename_bin(id, name.trim());
                    panel.renaming = None;
                    panel.rename_focus_sent = false;
                }
                return;
            }

            let label = ui.selectable_label(false, format!("[{bin_name}]"));
            if label.double_clicked() {
                panel.renaming = Some((bin_id, bin_name.to_string()));
            }
            label.context_menu(|ui| {
                if ui.button("Rename").clicked() {
                    panel.renaming = Some((bin_id, bin_name.to_string()));
                    panel.rename_focus_sent = false;
                    ui.close_menu();
                }
                if ui.button("Delete bin (keeps contents)").clicked() {
                    state.delete_bin(bin_id);
                    panel.expanded.remove(&bin_id);
                    ui.close_menu();
                }
                ui.separator();
                ui.label("Move this bin to:");
                if ui.button("(root)").clicked() {
                    state.move_bin(bin_id, None);
                    ui.close_menu();
                }
                let targets: Vec<(BinId, String)> = state
                    .project()
                    .bins
                    .iter()
                    .filter(|b| b.id != bin_id && !state.project().is_descendant_of(b.id, bin_id))
                    .map(|b| (b.id, b.name.clone()))
                    .collect();
                for (id, name) in targets {
                    if ui.button(&name).clicked() {
                        state.move_bin(bin_id, Some(id));
                        ui.close_menu();
                    }
                }
            });
        });
    });
}

/// `bin_label` is set only in search results, where rows appear out of tree
/// context and need to say which bin they came from.
fn draw_item_row(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    proxies: &mut crate::proxy_jobs::ProxyJobs,
    info: &ItemInfo,
    depth: usize,
    bin_label: Option<&str>,
) {
    // Stable widget id derived from *what* the row is, not where it sits.
    // egui auto-generates ids from layout order, and these rows reorder
    // whenever the sort changes — an unstable id means egui can't keep a
    // context menu attached to the row across frames, so the menu opens and
    // closes again immediately. Same fix as the timeline widget's clip ids.
    let row_id = match info.item {
        BinItem::Asset(id) => egui::Id::new(("project_item_asset", id.0)),
        BinItem::Sequence(id) => egui::Id::new(("project_item_sequence", id.0)),
    };
    ui.push_id(row_id, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 12.0 + 18.0);
            let selected = state.selected_item == Some(info.item);
            // No icon glyph: egui's bundled font doesn't cover the emoji this
            // would want (▶ 📁 🎬 all rendered as tofu boxes when tried), and the
            // Type column already says what each row is. Bundling an icon font
            // is the real fix if icons become worth having.
            let resp = ui.add_sized(
                [130.0, 18.0],
                egui::SelectableLabel::new(selected, &info.name),
            );
            if resp.clicked() {
                state.selected_item = Some(info.item);
            }
            if resp.double_clicked() {
                if let BinItem::Asset(id) = info.item {
                    state.selected_item = Some(info.item);
                    state.append_asset_to_timeline(id);
                }
            }
            resp.context_menu(|ui| {
                ui.label("Move to bin:");
                if ui.button("(root)").clicked() {
                    state.move_item_to_bin(info.item, None);
                    ui.close_menu();
                }
                let bins: Vec<(BinId, String)> = state
                    .project()
                    .bins
                    .iter()
                    .map(|b| (b.id, b.name.clone()))
                    .collect();
                for (id, name) in bins {
                    if ui.button(&name).clicked() {
                        state.move_item_to_bin(info.item, Some(id));
                        ui.close_menu();
                    }
                }
                if let BinItem::Asset(id) = info.item {
                    ui.separator();
                    if ui.button("Build proxy").clicked() {
                        if let Some(path) = state.asset_paths.get(&id).cloned() {
                            if !proxies.request(id, path) {
                                state.status =
                                    "this clip already has a proxy, or one is building".into();
                            }
                        }
                        ui.close_menu();
                    }
                }
            });
            ui.add_sized([60.0, 18.0], egui::Label::new(info.kind).truncate());
            // Proxy state column: blank for sequences and for clips with no
            // proxy, so it only draws attention when there's something to say.
            let proxy_label = match info.item {
                BinItem::Asset(id) => proxies.state_of(id).label(),
                BinItem::Sequence(_) => "",
            };
            ui.add_sized([28.0, 18.0], egui::Label::new(proxy_label).truncate());
            ui.add_sized(
                [55.0, 18.0],
                egui::Label::new(format_duration(info.duration_ticks)).truncate(),
            );
            ui.label(&info.detail);
            if let Some(bin) = bin_label {
                ui.weak(format!("in {bin}"));
            }
        });
    });
}

/// Flattened matches across every bin, so finding something never depends on
/// remembering where it was filed.
fn draw_search_results(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    panel: &mut ProjectPanelState,
    proxies: &mut crate::proxy_jobs::ProxyJobs,
) {
    let needle = panel.search.to_lowercase();
    let mut all: Vec<BinItem> = state
        .project()
        .assets
        .iter()
        .map(|a| BinItem::Asset(a.id))
        .collect();
    all.extend(
        state
            .project()
            .sequences
            .iter()
            .map(|s| BinItem::Sequence(s.id)),
    );

    // Each match carries its bin name so the label stays attached to its own
    // row through the sort.
    let mut matches: Vec<(ItemInfo, Option<String>)> = all
        .into_iter()
        .map(|i| item_info(state, i))
        .filter(|info| info.name.to_lowercase().contains(&needle))
        .map(|info| {
            let bin = state
                .project()
                .bin_of(info.item)
                .and_then(|id| state.project().bins.iter().find(|b| b.id == id))
                .map(|b| b.name.clone());
            (info, bin)
        })
        .collect();
    matches.sort_by(|(a, _), (b, _)| compare_items(panel, a, b));

    if matches.is_empty() {
        ui.weak(format!("No matches for \"{}\".", panel.search));
    }
    for (info, bin) in &matches {
        draw_item_row(ui, state, proxies, info, 0, bin.as_deref());
    }
}
