# Project file schema v1

Implementation: `crates/project/src/schema.rs` (types),
`crates/project/src/persist.rs` (save/load).

## Format

CBOR (via `ciborium`), chosen over FlatBuffers — see decisions-log.md.
File extension: `.nleproj` (placeholder — no user-facing naming decided yet).

## Top-level shape

```
ProjectDocument {
    schema_version: u32,              // = 1
    project: timeline::Project,       // sequences, tracks, clips, assets
    media_references: [MediaReference],
    undo_history: [PersistedCommand],
}
```

`media_references` implements spec 4.7's relink flow: each entry carries
`relative_path` (from the project file's location), `content_hash`, and
`original_absolute_path`. Relink resolution order (once M8 builds the actual
relink UI): try relative path, then content hash match anywhere under a
user-chosen search root, then prompt.

`undo_history` is bounded by whatever `max_history` the `command::UndoStack`
in the editing session used (default 100) — the schema itself doesn't cap
it; the writer does.

## Atomicity

`persist::save` writes to `<path>.tmp`, calls `File::sync_all()`, then
`std::fs::rename`s over the target. A crash between the temp write and the
rename leaves the original file untouched; a crash after the rename starts
is a single filesystem-level atomic operation on both Windows and POSIX
(same-volume rename). This is what spec 4.7's autosave requirement (write
temp, fsync, rename; never overwrite with a partial write) means concretely.

## Migration

`CURRENT_SCHEMA_VERSION = 1`. `persist::load` rejects any other version
outright — there is no migration chain yet because there is only one
version. The rule going forward (spec section 10's anti-pattern list): the
day `schema_version` becomes 2, a migration from 1 must ship in the same
change, operating on the CBOR value tree before final typed deserialization
so an old file never fails to parse just because a field was added or
renamed.

## What's deliberately not decided yet

- On-disk file layout (single file vs. a directory bundle holding the CBOR
  document plus a media-cache/proxy folder) — spec 4.7 leaves this
  `[DECIDE]` and it doesn't block M0's type work.
- OTIO/EDL export live in a separate module once M8 needs them; they are NOT
  part of this schema — OTIO is an interchange *export* format, not the
  native save format, and round-tripping through it is lossy by design.
