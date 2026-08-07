//! Project file schema v1, per spec 4.7. CBOR chosen over FlatBuffers (see
//! docs/decisions-log.md) — a document saved every 30s doesn't need
//! zero-copy reads, and CBOR versions more simply.

use media::MediaAssetId;
use serde::{Deserialize, Serialize};
use timeline::Project;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Media references per spec 4.7: relative path + content hash + original
/// absolute path, so "relink media" can match by hash when a path moves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaReference {
    pub asset_id: MediaAssetId,
    pub relative_path: String,
    pub content_hash: [u8; 32],
    pub original_absolute_path: String,
}

/// A single undo entry, persisted. Decision: undo history survives save/load
/// from v1.0 (see docs/decisions-log.md), bounded by whatever
/// `command::UndoStack::max_history` was in effect — enforced by the caller
/// before constructing this, not by this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedCommand {
    pub label: String,
    pub before: Project,
    pub after: Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDocument {
    pub schema_version: u32,
    pub project: Project,
    pub media_references: Vec<MediaReference>,
    pub undo_history: Vec<PersistedCommand>,
}

impl ProjectDocument {
    pub fn new(project: Project, media_references: Vec<MediaReference>) -> Self {
        ProjectDocument {
            schema_version: CURRENT_SCHEMA_VERSION,
            project,
            media_references,
            undo_history: Vec::new(),
        }
    }
}
