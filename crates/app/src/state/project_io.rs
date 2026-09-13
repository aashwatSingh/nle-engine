//! Bringing a project in and out: importing media, organising it into
//! bins, and saving/loading the document itself.

use super::*;

impl EditorState {
    pub fn import_assets(&mut self, paths: Vec<PathBuf>) {
        let mut project = (**self.project()).clone();
        let mut imported = 0;
        for path in paths {
            match media_ffmpeg::probe(&path) {
                Ok(asset) => {
                    self.asset_paths.insert(asset.id, path);
                    project.assets.push(asset);
                    imported += 1;
                }
                Err(e) => {
                    self.status = format!("failed to import {path:?}: {e:?}");
                }
            }
        }
        if imported > 0 {
            self.selected_item = project
                .assets
                .last()
                .map(|a| timeline::BinItem::Asset(a.id));
            self.undo.push("import media", std::sync::Arc::new(project));
        }
    }


    /// Finds (or lazily creates, via `apply_op`) the first track of `kind`,
    /// returning its id. Real NLEs let you manage tracks explicitly; this
    /// is the v1 stand-in — one V track and one A track, created on first
    /// use, rather than a full "insert track" UI.
    pub(super) fn ensure_track(&mut self, kind: TrackKind) -> TrackId {
        if let Some(t) = self.sequence().tracks.iter().find(|t| t.kind == kind) {
            return t.id;
        }
        let track_id = TrackId(self.next_id());
        let mut project = (**self.project()).clone();
        let seq = project
            .sequences
            .iter_mut()
            .find(|s| s.id == self.seq_id)
            .unwrap();
        let name = match kind {
            TrackKind::Video => format!(
                "V{}",
                seq.tracks
                    .iter()
                    .filter(|t| t.kind == TrackKind::Video)
                    .count()
                    + 1
            ),
            TrackKind::Audio => format!(
                "A{}",
                seq.tracks
                    .iter()
                    .filter(|t| t.kind == TrackKind::Audio)
                    .count()
                    + 1
            ),
        };
        seq.tracks.push(Track {
            id: track_id,
            kind,
            name,
            clips: vec![], transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
            locked: false,
            sync_locked: true,
            muted: false,
            solo: false,
            height_px: 60,
        });
        self.undo.push("add track", std::sync::Arc::new(project));
        track_id
    }


    /// Appends `asset` to the end of its kind's track (video assets with an
    /// audio stream get a linked audio clip too, mirroring how a real NLE
    /// treats a camera file as one linked A/V unit).
    pub fn append_asset_to_timeline(&mut self, asset_id: media::MediaAssetId) {
        let Some(asset) = self
            .project()
            .assets
            .iter()
            .find(|a| a.id == asset_id)
            .cloned()
        else {
            return;
        };
        let duration = TimeTick(asset.duration_ticks);
        self.adopt_settings_from_first_clip(&asset);

        if asset.video.is_some() {
            let track = self.ensure_track(TrackKind::Video);
            let at = self
                .sequence()
                .tracks
                .iter()
                .find(|t| t.id == track)
                .unwrap()
                .duration_end();
            let clip_id = ClipInstanceId(self.next_id());
            let clip = new_clip(clip_id, asset.id, duration);
            self.apply_op("append clip", EditOp::Overwrite { track, at, clip });
        }
        if asset.audio.is_some() {
            let track = self.ensure_track(TrackKind::Audio);
            let at = self
                .sequence()
                .tracks
                .iter()
                .find(|t| t.id == track)
                .unwrap()
                .duration_end();
            let clip_id = ClipInstanceId(self.next_id());
            let clip = new_clip(clip_id, asset.id, duration);
            self.apply_op("append audio clip", EditOp::Overwrite { track, at, clip });
        }
    }

    // --- Project panel: bins ---------------------------------------------
    //
    // Bin edits go through the undo stack like every other change, so
    // organising media is undoable and a mis-drop is one Ctrl+Z away. They
    // clone-mutate-push directly rather than going through
    // `timeline::edit_ops`, for the same reason effect edits do: the
    // invariants that op set enforces are about clip positions on tracks,
    // and none of them apply to folder membership.


    /// Creates a bin under `parent` and returns its id.
    pub fn create_bin(&mut self, name: &str, parent: Option<timeline::BinId>) -> timeline::BinId {
        let id = timeline::BinId(self.next_id());
        let mut project = (**self.project()).clone();
        project.bins.push(timeline::Bin {
            id,
            name: name.to_string(),
            parent,
            items: Vec::new(),
        });
        self.undo.push("new bin", std::sync::Arc::new(project));
        id
    }


    pub fn rename_bin(&mut self, id: timeline::BinId, name: &str) {
        let mut project = (**self.project()).clone();
        if let Some(bin) = project.bins.iter_mut().find(|b| b.id == id) {
            if bin.name == name {
                return; // no-op; don't add an undo step for it
            }
            bin.name = name.to_string();
            self.undo.push("rename bin", std::sync::Arc::new(project));
        }
    }


    /// Moves `item` into `target` (or to the root when `None`).
    pub fn move_item_to_bin(&mut self, item: timeline::BinItem, target: Option<timeline::BinId>) {
        if self.project().bin_of(item) == target {
            return;
        }
        let mut project = (**self.project()).clone();
        // Remove from wherever it currently is first — an item belongs to
        // exactly one bin, and letting it appear in two would make
        // `root_items` and `bin_of` disagree about where it lives.
        for bin in &mut project.bins {
            bin.items.retain(|i| *i != item);
        }
        if let Some(target) = target {
            if let Some(bin) = project.bins.iter_mut().find(|b| b.id == target) {
                bin.items.push(item);
            }
        }
        self.undo.push("move to bin", std::sync::Arc::new(project));
    }


    /// Reparents a bin, refusing moves that would create a cycle.
    pub fn move_bin(&mut self, bin: timeline::BinId, new_parent: Option<timeline::BinId>) {
        if let Some(parent) = new_parent {
            if self.project().is_descendant_of(parent, bin) {
                self.status = "can't move a bin into itself".into();
                return;
            }
        }
        let mut project = (**self.project()).clone();
        if let Some(b) = project.bins.iter_mut().find(|b| b.id == bin) {
            if b.parent == new_parent {
                return;
            }
            b.parent = new_parent;
            self.undo.push("move bin", std::sync::Arc::new(project));
        }
    }


    /// Deletes a bin, promoting its contents to the bin's own parent rather
    /// than deleting them. Removing media from the project is a separate,
    /// much more destructive action — a folder delete that silently took the
    /// footage with it would be a data-loss trap.
    pub fn delete_bin(&mut self, id: timeline::BinId) {
        let mut project = (**self.project()).clone();
        let Some(index) = project.bins.iter().position(|b| b.id == id) else {
            return;
        };
        let removed = project.bins.remove(index);
        let grandparent = removed.parent;
        for child in project.bins.iter_mut().filter(|b| b.parent == Some(id)) {
            child.parent = grandparent;
        }
        // A `None` grandparent means the items are at the root by
        // definition, so dropping them from the tree is all that's needed.
        if let Some(parent_id) = grandparent {
            if let Some(parent) = project.bins.iter_mut().find(|b| b.id == parent_id) {
                parent.items.extend(removed.items);
            }
        }
        self.undo.push("delete bin", std::sync::Arc::new(project));
    }


    /// Display name for a Project-panel item.
    pub fn item_name(&self, item: timeline::BinItem) -> String {
        match item {
            timeline::BinItem::Asset(id) => self
                .project()
                .assets
                .iter()
                .find(|a| a.id == id)
                .and_then(|a| std::path::Path::new(&a.original_absolute_path).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "(missing asset)".into()),
            timeline::BinItem::Sequence(id) => self
                .project()
                .sequences
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "(missing sequence)".into()),
        }
    }


    /// When the very first clip lands on an empty timeline, resize the
    /// sequence to match it — what Premiere and Resolve both offer on the
    /// first drop, and for the same reason: the default 1920x1080 sequence
    /// otherwise renders a 640x360 phone clip as a small island in a large
    /// black frame, both in the preview and in the exported file. Adopting
    /// the source's dimensions makes the common case (one camera format)
    /// right by default, and only applies while nothing is on the timeline
    /// yet, so it can never silently re-frame an edit in progress.
    fn adopt_settings_from_first_clip(&mut self, asset: &media::MediaAsset) {
        let already_has_clips = self.sequence().tracks.iter().any(|t| !t.clips.is_empty());
        if already_has_clips {
            return;
        }
        let Some(video) = &asset.video else { return };
        let rate = standard_frame_rate(&video.frame_rate);
        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else {
            return;
        };
        if seq.settings.width == video.width
            && seq.settings.height == video.height
            && rate.is_none_or(|r| r == seq.settings.frame_rate)
        {
            return; // nothing to change; don't push a no-op undo step
        }
        seq.settings.width = video.width;
        seq.settings.height = video.height;
        if let Some(rate) = rate {
            seq.settings.frame_rate = rate;
            seq.settings.drop_frame_timecode = rate.is_drop_frame_by_default();
        }
        self.undo
            .push("match sequence to clip", std::sync::Arc::new(project));
    }


    /// Saves to `path` via `project::save` (atomic write-temp-fsync-rename,
    /// already implemented and tested in the `project` crate).
    ///
    /// Media references carry the content hash and both a relative and the
    /// original absolute path, per spec 4.7, so a moved project folder can
    /// still relink by hash. `relative_path` is computed against the project
    /// file's own directory when the media lives at or below it, which is
    /// the case that actually benefits from relative paths (a
    /// self-contained project folder you can move or copy wholesale);
    /// media elsewhere on disk falls back to its absolute path, since
    /// inventing a `../../..` chain across volumes wouldn't survive a move
    /// either and would just obscure where the file really is.
    /// Writes the project to `path` **without** claiming it as the project's
    /// own location or touching the status line.
    ///
    /// Autosave uses this: adopting the recovery file as `project_path` would
    /// mean the next Ctrl+S silently saved over the recovery file instead of the
    /// user's project, and a status line churning every 20 seconds would bury
    /// whatever the user was actually being told.
    pub fn write_snapshot_to(&self, path: &std::path::Path) -> Result<(), String> {
        // No undo history in a recovery write. `docs/decisions-log.md` bounds
        // history specifically to keep autosave size predictable; excluding it
        // outright is stricter and cheaper still, because serialising N project
        // snapshots every 20 seconds would make the cost of autosave scale with
        // how long you'd been working — the opposite of what a background safety
        // net should do. A crash therefore loses undo history but not work.
        let doc = self.build_document(path, false);
        project::save(&doc, path).map_err(|e| format!("{e:?}"))
    }


    /// Builds the on-disk document, with media references made relative to
    /// `path`'s directory.
    ///
    /// `include_history` persists the undo stack (spec 4.3 /
    /// `docs/decisions-log.md` 2026-08-07: undo history is persisted from v1.0,
    /// bounded by `UndoStack::max_history`).
    fn build_document(
        &self,
        path: &std::path::Path,
        include_history: bool,
    ) -> project::ProjectDocument {
        let project = (**self.project()).clone();
        let base_dir = path.parent();
        let media_references = project
            .assets
            .iter()
            .map(|a| {
                let abs = std::path::Path::new(&a.original_absolute_path);
                let relative_path = base_dir
                    .and_then(|dir| abs.strip_prefix(dir).ok())
                    .map(|rel| rel.to_string_lossy().into_owned())
                    .unwrap_or_else(|| a.original_absolute_path.clone());
                project::MediaReference {
                    asset_id: a.id,
                    relative_path,
                    content_hash: a.content_hash,
                    original_absolute_path: a.original_absolute_path.clone(),
                }
            })
            .collect();
        let mut doc = project::ProjectDocument::new(project, media_references);
        if include_history {
            // Only the most recent entries, and only each one's `before` — the
            // `after` is recoverable from the next entry (see
            // `project::PersistedCommand`). Together these took a 40-clip
            // project's file from 199x its bare size down to a few times it.
            let history = self.undo.history();
            let keep = history.len().saturating_sub(project::MAX_PERSISTED_UNDO_ENTRIES);
            doc.undo_history = history[keep..]
                .iter()
                .map(|e| project::PersistedCommand {
                    label: e.label.clone(),
                    before: (*e.before).clone(),
                })
                .collect();
        }
        doc
    }


    pub fn save_to(&mut self, path: &std::path::Path) {
        let doc = self.build_document(path, true);
        match project::save(&doc, path) {
            Ok(()) => {
                self.project_path = Some(path.to_path_buf());
                self.status = format!("saved to {}", path.display());
            }
            Err(e) => self.status = format!("save failed: {e:?}"),
        }
    }


    /// Loads a project, replacing all current state. Returns false (and
    /// leaves the editor untouched) if the file can't be read — an
    /// unreadable file must not destroy whatever the user already has open.
    pub fn open_from(&mut self, path: &std::path::Path) -> bool {
        let doc = match project::load(path) {
            Ok(doc) => doc,
            // Worth its own wording: every other open failure is about the
            // file being unreadable, and telling someone their intact,
            // openable project "failed to open" with a struct dump invites
            // them to go looking for a disk problem that isn't there.
            Err(project::LoadError::Corrupt(violation)) => {
                self.status = format!(
                    "couldn't open {} — the file describes a damaged project ({violation:?})",
                    path.display()
                );
                return false;
            }
            Err(e) => {
                self.status = format!("open failed: {e:?}");
                return false;
            }
        };

        // Resolve each asset back to a real file so the preview can decode
        // it: try the saved relative path against this file's directory
        // first (so a moved-but-self-contained project folder just works),
        // then the recorded absolute path. Missing media isn't fatal —
        // the project still opens, those clips just render as gaps, which
        // is the honest outcome and matches what "relink media" exists to
        // fix. Hash-based relinking (searching a directory for a matching
        // content_hash) is the real fix and isn't built yet.
        let base_dir = path.parent();
        let mut missing = 0;
        self.asset_paths.clear();
        // A freshly loaded project's clip ids restart from a low number
        // (see `next_id` below), so a transcript cached against the
        // previous project's clip ids could otherwise silently attach
        // itself to an unrelated clip that happens to reuse the same id.
        self.transcripts.clear();
        // Same collision, worse outcome: an analysis still running for the
        // old project would razor or re-gain whichever new clip reuses its
        // id. A fresh channel orphans those workers; what they send is dropped.
        self.analysis = super::analysis_jobs::AnalysisJobs::default();
        for r in &doc.media_references {
            let candidate = base_dir
                .and_then(|d| resolve_relative_media(d, &r.relative_path))
                .filter(|p| p.exists())
                .or_else(|| {
                    let abs = PathBuf::from(&r.original_absolute_path);
                    abs.exists().then_some(abs)
                });
            match candidate {
                Some(p) => {
                    self.asset_paths.insert(r.asset_id, p);
                }
                None => missing += 1,
            }
        }

        // Every ID in the loaded project must be below `next_id`, or newly
        // created clips/tracks would collide with existing ones — a
        // collision would make `find_clip`-style lookups match the wrong
        // object and corrupt edits in ways that are painful to trace back
        // to their cause.
        self.next_id = max_id_in(&doc.project) + 1;

        self.seq_id = doc
            .project
            .sequences
            .first()
            .map(|s| s.id)
            .unwrap_or(SequenceId(1));
        // Restore the undo stack, so reopening a project doesn't silently lose
        // the ability to undo the work in it (spec 4.3 /
        // `docs/decisions-log.md` 2026-08-07).
        //
        // The chain is *reconstructed* rather than read verbatim: each entry's
        // `after` is the next entry's `before`, and the last one's is the
        // project itself. Schema v3 stored `after` too and this code validated
        // the two against each other, dropping the history when they disagreed.
        // Not storing it is better than checking it — the inconsistency it
        // guarded against is now unrepresentable, and the file is half the size.
        let current = std::sync::Arc::new(doc.project);
        let befores: Vec<std::sync::Arc<Project>> = doc
            .undo_history
            .iter()
            .map(|c| std::sync::Arc::new(c.before.clone()))
            .collect();
        let history: Vec<command::CommandEntry> = doc
            .undo_history
            .iter()
            .enumerate()
            .map(|(i, c)| command::CommandEntry {
                label: c.label.clone(),
                before: befores[i].clone(),
                after: befores.get(i + 1).cloned().unwrap_or_else(|| current.clone()),
            })
            .collect();
        self.undo = command::UndoStack::restore(
            current,
            history,
            command::UndoStack::DEFAULT_MAX_HISTORY,
        );
        self.playhead = 0;
        self.selected_clips.clear();
        self.selected_item = None;
        // Marks are positions in a sequence, so they carry no meaning into a
        // different one — and they aren't inert: `start_export` scopes a
        // range export to them.
        self.clear_marks();
        self.playing = false;
        self.play_anchor = None;
        self.drag = None;
        self.scroll_ticks = 0;
        self.project_path = Some(path.to_path_buf());
        // The "undo history was discarded as inconsistent" branches are gone
        // with schema v4: a reconstructed chain cannot disagree with its
        // project, so there is no such outcome to report.
        self.status = match missing {
            0 => format!("opened {}", path.display()),
            n => format!("opened {} ({n} media file(s) not found)", path.display()),
        };
        true
    }
}

/// Joins a saved relative media path onto the project's own folder, or
/// `None` if it isn't the relative path it claims to be.
///
/// `relative_path` exists so a self-contained project folder can be moved
/// or copied wholesale and still find its footage. A project file is
/// ordinary untrusted input — it can arrive by download or email like any
/// document — and `Path::join` silently discards the base when handed an
/// absolute path, so without this check the field marked "relative" could
/// name any file on the machine and the editor would try to decode it.
/// `..` gets the same treatment for the same reason.
///
/// Media genuinely stored outside the project folder isn't affected: that
/// case is what `original_absolute_path` is for, and the caller still
/// falls back to it.
pub(super) fn resolve_relative_media(base: &std::path::Path, relative: &str) -> Option<PathBuf> {
    use std::path::Component;
    let relative = std::path::Path::new(relative);
    let escapes = relative.components().any(|c| {
        matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))
    });
    (!relative.is_absolute() && !escapes).then(|| base.join(relative))
}
