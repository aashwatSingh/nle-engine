# Drag-and-drop media import

## Goal

Importing media today requires clicking Import and going through a file
picker. Add drag-and-drop: drop a file anywhere on the app window and it
imports into the project bin, the same as clicking Import — just without
the dialog.

## Design

- Handle `WindowEvent::DroppedFile(path)` in the existing winit event
  match in `main.rs`, calling `state.import_assets(vec![path])` — the
  exact function the Import button already calls (`project_panel.rs`),
  so dedup/thumbnailing/etc. are reused unchanged. One call per dropped
  file (winit fires one `DroppedFile` event per file in a multi-file
  drop).
- Only active when `screen == Screen::Editor`; drops while on the Home
  screen are ignored — there's no project open to import into.
- `WindowEvent::HoveredFile` sets a `dragging_file: bool`, drawn as a
  subtle overlay border while true; `WindowEvent::HoveredFileCancelled`
  and `DroppedFile` both clear it back to `false`.
- No new dependencies — winit 0.29 (already a dependency) has all three
  event variants natively.

## Explicitly out of scope

Confirmed with the user: dropping directly onto the timeline to
auto-place the clip (not just import it) is not part of this pass — it
would need to determine track/position and handle overlaps, which is
meaningfully more work than a bin-only import.

## Testing

No new pure logic to unit test — this is event wiring around an existing,
already-tested function. Verified live: drag a file from Explorer onto
the app window, confirm it lands in the project bin; confirm the overlay
shows while dragging and clears on drop; confirm a drop on the Home
screen is a no-op.
