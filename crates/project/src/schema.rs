//! Project file schema v1, per spec 4.7. CBOR chosen over FlatBuffers (see
//! docs/decisions-log.md) — a document saved every 30s doesn't need
//! zero-copy reads, and CBOR versions more simply.

use media::MediaAssetId;
use serde::{Deserialize, Serialize};
use timeline::Project;

/// Version history:
/// - **2**: `timeline::Project` gained `bins` (Project-panel folders).
/// - **3**: `timeline::Track` gained `transitions`, `gain_db` and `pan`.
/// - **4**: `PersistedCommand` dropped its redundant `after` snapshot.
///
/// Both added fields are `#[serde(default)]`, so older documents still
/// deserialize. The version bumps exist so the migration chain has a recorded
/// step per change, and so a *future* breaking change can tell the generations
/// apart — a format change without a migration step is the thing spec 4.7
/// forbids. See `persist::migrate`.
pub const CURRENT_SCHEMA_VERSION: u32 = 6;

/// The oldest version this build can still open. Anything below it is
/// rejected with a clear error rather than silently mis-read.
pub const OLDEST_SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// Media references per spec 4.7: relative path + content hash + original
/// absolute path, so "relink media" can match by hash when a path moves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaReference {
    pub asset_id: MediaAssetId,
    pub relative_path: String,
    pub content_hash: [u8; 32],
    pub original_absolute_path: String,
}

/// How many undo entries a saved project carries.
///
/// Far fewer than the 100 the in-memory stack holds. Undo depth is most
/// valuable in the minutes after an edit; the hundredth step back from a
/// reopened project is worth very little, and every entry is a whole project
/// snapshot. Measured: at 100 entries a 32 KB project saved as 6.4 MB.
pub const MAX_PERSISTED_UNDO_ENTRIES: usize = 25;

/// A single undo entry, persisted. Decision: undo history survives save/load
/// from v1.0 (see docs/decisions-log.md).
///
/// **Only the state *before* the command is stored.** The state after it is
/// the next entry's `before`, and the last entry's is
/// `ProjectDocument::project` — so storing an `after` too (which schema v3 did)
/// doubled the file for zero information. Beyond the size win, it also makes an
/// inconsistent history *unrepresentable*: there is no longer a redundant copy
/// that can disagree with the project, so there is nothing to validate and no
/// "history discarded" failure mode to handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedCommand {
    pub label: String,
    pub before: Project,
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
