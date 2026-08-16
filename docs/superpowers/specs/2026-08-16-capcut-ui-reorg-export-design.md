# CapCut-style right panel reorg + export dialog with resolution scaling

## Goal

Two independent, small UI improvements requested together:

1. The right-side panel (Audio Mixer, Title panel, Effects, Transcript,
   Scopes) is one long vertical stack — reaching Scopes today means
   scrolling past everything above it. Reorganize it into tabs.
2. Export options (quality preset, range) are currently two controls
   buried inside the File dropdown menu. Move them into a real Export
   dialog, and add resolution scaling (Native/75%/50%/25%) as the one new
   option.

Both are scoped narrowly on purpose — this is a UI/ergonomics pass, not a
rearchitecture of either the panel system or the export pipeline.

## Part 1: Tabbed right panel

### Problem

`build_ui`'s right `SidePanel` currently draws, top to bottom: a
collapsible "Audio Mixer" header, the title panel (conditional — only
draws when a title clip is selected), the effects panel, the transcript
panel, then the scopes panel. All five are always in the layout
simultaneously; getting to Scopes means scrolling past Effects and
Transcript first.

### Design

- New `RightPanelTab` enum: `Effects | Transcript | Scopes | Mixer`.
- New small state struct (not folded into `EffectsPanelState`, since it
  isn't effects-specific): tracks the currently selected tab.
- The title panel stays pinned above the tab strip, unconditionally in
  the layout — it's a "shows when relevant" panel, not something a user
  tabs away from mid-edit, and it was never part of the scrolling problem
  (it draws nothing when no title is selected).
- Below the title panel: a horizontal row of four tab buttons, styled
  with the existing hand-drawn icon set (`icons.rs`) the same way the
  toolbar already does, followed by a `match` on the selected tab calling
  exactly one of the four panels' existing `show` functions.
- No changes to `effects_panel.rs`, `transcript_panel.rs`, `scopes_panel.rs`,
  or the mixer's `show` logic — they already take `ui: &mut egui::Ui` and
  have no opinion about their container.

### Testing

Pure layout change, no new logic to unit test. Verified live: click each
of the four tabs, confirm the right content shows and the others don't,
confirm Effects still works mid-edit (add an effect, keyframe a param)
without anything else on screen fighting it for space.

## Part 2: Export dialog + resolution scaling

### Problem

Today, `File` menu contains inline `QualityPreset` radio buttons, a range
checkbox, and an "Export Video..." button that immediately opens the
save-file dialog and starts the export. There's no options surface beyond
those two controls, and no dedicated dialog the way CapCut/Premiere both
have.

### Design

**UI flow.** File > "Export..." opens a new modal `egui::Window`
("Export") containing:
- Resolution: radio buttons, `Native | 75% | 50% | 25%`.
- Quality: today's `QualityPreset::ALL` radios, moved in unchanged.
- Range: today's "Only the in/out range" checkbox, moved in unchanged
  (same enable/disable-on-no-marks behavior as today).
- An "Export..." button. Clicking it closes this dialog, opens the
  existing `rfd::FileDialog` save prompt, and starts the job exactly as
  today (`start_export`) — no change to that mechanism, just what
  triggers it.

The existing progress/result window (`export_ui`) is unchanged; it takes
over once the job starts, same as today.

**Resolution scaling — approach.** Two were considered:

- *(chosen)* **Post-render downscale.** Export renders at the sequence's
  native resolution exactly as today — same `GraphCompiler`, same
  `Compositor::render_to_rgba` call preview itself uses — then, if a
  non-Native scale was chosen, runs the resulting RGBA through an
  `ffmpeg_next::software::scaling::Context` (the same primitive
  `matte_video.rs` and `proxy.rs` already use for their own downscaling)
  before handing it to the encoder. Touches only the `export` crate.
- *(rejected)* **Compile the graph at the scaled size directly.** Would
  need the compositor to render at an arbitrary target size rather than
  the sequence's own. Riskier: some effects may have pixel-space
  assumptions (blur radius, mask geometry) that aren't perfectly
  resolution-independent, which could make export stop matching what
  preview shows at 100% — undermining `export`'s own documented
  "what you see is what you get" guarantee. Rejected without spending
  time verifying that guarantee first, since the post-render approach
  gets the same user-visible result with materially less risk.

**Data model.**
- `ExportOptions` gains `pub output_scale: OutputScale`, where
  `OutputScale` is `enum { Native, Percent(u32) }` — a closed enum rather
  than a raw float or percentage integer, so a nonsense scale can't be
  constructed, matching why `QualityPreset` is already a closed enum
  rather than a raw CRF number.
- `ExportUi` (the app-side struct holding what the dialog edits) gains a
  matching `output_scale: OutputScale` field, defaulting to `Native`.

**Render loop change (`export_sequence`).**
- `width`/`height` are computed from `sequence.settings` exactly as
  today (this is still what the graph compiles and renders at).
- A new `(out_width, out_height)` is computed by applying
  `options.output_scale` to `width`/`height`, even-rounded the same way
  the native size already is (`n + (n % 2)`) — required for yuv420p
  chroma subsampling, same reasoning as the existing rounding.
- `Encoder::open` takes `(out_width, out_height)` instead of
  `(width, height)` — it already takes width/height as plain parameters,
  so this is a call-site change, not a signature change.
- Between `compositor.render_to_rgba(...)` (still rendered at native
  `width`/`height`) and `encoder.write_frame(...)`: if
  `(out_width, out_height) != (width, height)`, scale the RGBA frame down
  via `ffmpeg_next::software::scaling::Context` before writing. At
  `Native`, this comparison is false and the step is skipped entirely —
  the default path has zero behavior change from today.

### Testing

New test in `export`'s existing style: export a real small sequence with
`OutputScale::Percent(50)`, probe the output file, assert its dimensions
are exactly the even-rounded half of the sequence's native size. Existing
export tests are unaffected — `OutputScale::Native` is the default and is
behaviorally identical to current code (the new scaling branch is never
taken).

### Explicitly out of scope

Confirmed with the user, not being built in this pass:
- Frame rate override (export always uses the sequence's own frame rate).
- Manual CRF/bitrate entry (`QualityPreset`'s three presets remain the
  only way to pick quality).
- Custom (non-percentage, arbitrary WxH) resolution entry.
- Format/codec choice beyond today's H.264 + AAC + mp4.

## Summary of touched files

- `crates/app/src/main.rs` — right panel tab strip + `RightPanelTab`
  enum/state (defined directly in `main.rs`, same as the existing
  `Screen` enum — small enough not to warrant its own module); new Export
  dialog window; `ExportUi` gains `output_scale`; File menu's inline
  quality/range controls removed in favor of opening the dialog.
- `crates/export/src/lib.rs` — `ExportOptions::output_scale`,
  `OutputScale` enum, scaled-dimension computation, conditional
  post-render downscale step, one new test.
