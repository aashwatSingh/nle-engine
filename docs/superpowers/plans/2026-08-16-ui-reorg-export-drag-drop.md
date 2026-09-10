# Right-panel tabs, export dialog, and drag-and-drop import — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reorganize the editor's right-side panel into tabs, replace the
File-menu export controls with a real Export dialog that adds resolution
scaling, and add drag-and-drop media import.

**Architecture:** Three independent, small changes to the `app` crate's
`main.rs` (plus one supporting change to the `export` crate for
resolution scaling). None of them touch the shared render/compositor
pipeline — resolution scaling reuses the encoder's existing RGBA→YUV420P
scaler by giving it different source/destination dimensions instead of
adding a new scaling pass; the panel tabs and drag-and-drop are pure UI
wiring around functions that already exist and are already tested.

**Tech Stack:** Rust, egui 0.28 (UI), winit 0.29 (window/input events),
ffmpeg-next 7.1 (encoder), existing `export`/`app` crates.

## Global Constraints

- Every new export option must be a closed enum, not a raw number — same
  reasoning as the existing `QualityPreset`: a caller can't construct a
  nonsense value. (Source: `docs/superpowers/specs/2026-08-16-capcut-ui-reorg-export-design.md`)
- Resolution scaling must not change the `Native`-scale code path's
  behavior at all — no new work, no new branch taken, when
  `OutputScale::Native` is used (the default). (Source: same spec)
- Frame rate override, manual CRF/bitrate entry, custom (non-percentage)
  resolution, and format/codec choice are explicitly OUT of scope for
  this plan. (Source: same spec)
- Drag-and-drop only imports to the project bin — it does not also place
  the clip on the timeline, and it is a no-op on the Home screen.
  (Source: `docs/superpowers/specs/2026-08-16-drag-and-drop-import-design.md`)
- This project's established testing convention: real logic gets a real
  test (TDD, red-first where practical); pure egui layout/wiring changes
  with no new logic are verified live in the running app instead of via
  contrived unit tests. Follow this convention — do not invent fake tests
  for layout-only code.

---

## File Structure

- **Modify `crates/export/src/lib.rs`**: new `OutputScale` enum (pure,
  unit-tested); `ExportOptions` gains an `output_scale` field;
  `export_sequence`'s dimension computation and `Encoder::open`'s
  signature both thread the scaled output size through; `ExportStats`
  reports the actual (possibly scaled) output dimensions.
- **Modify `crates/export/tests/export_sequence.rs`**: one new
  integration test exporting at 50% scale and probing the real output
  file's dimensions.
- **Modify `crates/app/src/main.rs`**:
  - `ExportUi` gains `output_scale` and `show_dialog` fields; new
    `export_dialog_ui` function (the pre-export options window); File
    menu's inline quality/range controls and "Export Video..." button
    are replaced by a single "Export..." item that opens the dialog.
  - New `RightPanelTab` enum + `RightPanelState` struct; the right
    `SidePanel` closure is rewritten from five stacked panels to a tab
    strip + one matched panel.
  - New `dragging_file: bool` local in `main()`; three new
    `WindowEvent` match arms (`DroppedFile`, `HoveredFile`,
    `HoveredFileCancelled`); `build_ui` gains a `dragging_file: bool`
    parameter and draws a border overlay while true.

No new files. No new dependencies — winit 0.29 and egui 0.28 (both
already dependencies) provide everything needed.

---

## Task 1: `OutputScale` enum

**Files:**
- Modify: `crates/export/src/lib.rs` (add near `QualityPreset`, around
  line 76)

**Interfaces:**
- Produces: `pub enum OutputScale { Native, Percent(u32) }` with
  `pub const ALL: [OutputScale; 4]`, `pub fn label(self) -> &'static str`,
  and `pub fn scaled_dimensions(self, width: u32, height: u32) -> (u32, u32)`.
  Task 2 calls `scaled_dimensions`; Task 3 (app crate) uses `ALL` and
  `label`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block at the bottom of
`crates/export/src/lib.rs` (after `frame_count_rounds_up_partial_final_frame`):

```rust
    #[test]
    fn scaled_dimensions_are_even_and_proportional() {
        assert_eq!(OutputScale::Native.scaled_dimensions(1920, 1080), (1920, 1080));
        assert_eq!(OutputScale::Percent(50).scaled_dimensions(1920, 1080), (960, 540));
        assert_eq!(OutputScale::Percent(75).scaled_dimensions(1920, 1080), (1440, 810));
        // 103*50/100 truncates to 51 (odd) -> must round up to 52.
        assert_eq!(OutputScale::Percent(50).scaled_dimensions(103, 100), (52, 50));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p export scaled_dimensions_are_even_and_proportional`
Expected: FAIL to compile — `OutputScale` does not exist yet.

- [ ] **Step 3: Write minimal implementation**

Add directly above the `#[derive(Debug)] pub struct ExportOptions` in
`crates/export/src/lib.rs` (around line 78):

```rust
/// Export resolution relative to the sequence's native size. A closed
/// enum rather than a raw percentage or explicit width/height, for the
/// same reason `QualityPreset` is a closed enum rather than a raw CRF
/// number: a caller can't construct a nonsense value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputScale {
    /// The sequence's own resolution — the default, and the only value
    /// that must produce byte-identical behavior to code that predates
    /// this enum.
    Native,
    Percent(u32),
}

impl OutputScale {
    pub const ALL: [OutputScale; 4] =
        [OutputScale::Native, OutputScale::Percent(75), OutputScale::Percent(50), OutputScale::Percent(25)];

    pub fn label(self) -> &'static str {
        match self {
            OutputScale::Native => "Native",
            OutputScale::Percent(75) => "75%",
            OutputScale::Percent(50) => "50%",
            OutputScale::Percent(25) => "25%",
            OutputScale::Percent(_) => "Custom",
        }
    }

    /// Applies this scale to `(width, height)`, rounding up to even
    /// dimensions — required for yuv420p 4:2:0 chroma subsampling, same
    /// convention `export_sequence` already applies to the native size.
    pub fn scaled_dimensions(self, width: u32, height: u32) -> (u32, u32) {
        let pct = match self {
            OutputScale::Native => return (width, height),
            OutputScale::Percent(p) => p,
        };
        let even = |n: u32| if n % 2 == 0 { n.max(2) } else { n + 1 };
        let scaled_w = (width as u64 * pct as u64 / 100) as u32;
        let scaled_h = (height as u64 * pct as u64 / 100) as u32;
        (even(scaled_w), even(scaled_h))
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p export scaled_dimensions_are_even_and_proportional`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cd crates/export
git add src/lib.rs
git commit -m "Add OutputScale enum for export resolution scaling"
```

---

## Task 2: Wire `OutputScale` into `export_sequence` and `Encoder`

**Files:**
- Modify: `crates/export/src/lib.rs`
  - `ExportOptions` struct (line ~79) and its `Default` impl (line ~93)
  - `export_sequence`'s dimension setup (lines ~239–264) and `ExportStats`
    construction (lines ~299–308)
  - `Encoder::open` (lines ~441–509) and `Encoder::write_frame` (lines
    ~518–540, uses `self.width`/`self.height`)
- Modify: `crates/export/tests/export_sequence.rs`
  - Import list (line 7)
  - New test after `exports_a_real_playable_video_with_the_sequence_dimensions_and_length`

**Interfaces:**
- Consumes: `OutputScale::scaled_dimensions` from Task 1.
- Produces: `ExportOptions.output_scale: OutputScale` (Task 3 sets this
  from the dialog). `ExportStats.width`/`.height` now report the actual
  output dimensions (may differ from the sequence's native size).

- [ ] **Step 1: Write the failing test**

Add to `crates/export/tests/export_sequence.rs`, after the
`exports_a_real_playable_video_with_the_sequence_dimensions_and_length`
test (around line 176):

```rust
#[test]
fn exporting_at_half_scale_produces_a_half_sized_file() {
    let duration = timeline::TIMEBASE; // 1 second
    let (project, paths) = setup(duration);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.mp4");

    let stats = export_sequence(
        &project,
        SEQ,
        &paths,
        &out,
        &ExportOptions {
            quality: QualityPreset::Draft,
            output_scale: OutputScale::Percent(50),
            ..Default::default()
        },
        |_, _| true,
    )
    .expect("export should succeed");

    // The sequence in `setup` is 640x360; 50% is exactly 320x180, already even.
    assert_eq!((stats.width, stats.height), (320, 180));
    let probed = media_ffmpeg::probe(&out).expect("output must be a probe-able video");
    let video = probed.video.expect("output must have a video stream");
    assert_eq!((video.width, video.height), (320, 180), "the real file, not just the stats, must be half-sized");
}
```

Change line 7's import to add `OutputScale`:

```rust
use export::{export_sequence, ExportError, ExportOptions, OutputScale, QualityPreset};
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p export --test export_sequence exporting_at_half_scale_produces_a_half_sized_file`
Expected: FAIL to compile — `ExportOptions` has no `output_scale` field yet.

- [ ] **Step 3: Write minimal implementation**

In `crates/export/src/lib.rs`, add the field to `ExportOptions` (around
line 79):

```rust
#[derive(Debug)]
pub struct ExportOptions {
    pub quality: QualityPreset,
    /// Output audio sample rate. The mixer resamples every source to this.
    pub sample_rate: u32,
    /// Master bus gain applied after summing, in dB.
    pub master_gain_db: f64,
    /// Sequence range to render, in ticks. `None` exports the whole sequence.
    ///
    /// Output timestamps always start at zero regardless — a range export is a
    /// standalone file, not a clip that begins several seconds in with nothing
    /// on screen.
    pub range_ticks: Option<(i64, i64)>,
    /// Output resolution relative to the sequence's native size.
    pub output_scale: OutputScale,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            quality: QualityPreset::High,
            sample_rate: 48_000,
            master_gain_db: 0.0,
            range_ticks: None,
            output_scale: OutputScale::Native,
        }
    }
}
```

Change the dimension setup in `export_sequence` (around lines 239–264)
from:

```rust
    let width = sequence.settings.width + (sequence.settings.width % 2);
    let height = sequence.settings.height + (sequence.settings.height % 2);

    let (device, queue) = render::headless_context().ok_or(ExportError::NoGpu)?;
    let compositor = Compositor::new(device, queue);
    let compiler = GraphCompiler::new(BuiltinRegistry::default());

    // No audio clips means no audio stream at all. A silent AAC track would
    // be indistinguishable, in the file, from "the mix failed" — better to
    // let the absence be the signal.
    let has_audio = sequence
        .tracks
        .iter()
        .any(|t| t.kind == TrackKind::Audio && !t.clips.is_empty());

    let mut encoder = Encoder::open(
        output,
        width,
        height,
        sequence.settings.frame_rate,
        options,
        has_audio,
    )?;
```

to:

```rust
    let width = sequence.settings.width + (sequence.settings.width % 2);
    let height = sequence.settings.height + (sequence.settings.height % 2);
    // The compositor still renders at the sequence's native size (below) —
    // only the encoder's output size changes. This is what keeps
    // export/preview pixel-identical at `OutputScale::Native` and avoids
    // any risk of effects behaving differently at a scaled render target.
    let (out_width, out_height) = options.output_scale.scaled_dimensions(width, height);

    let (device, queue) = render::headless_context().ok_or(ExportError::NoGpu)?;
    let compositor = Compositor::new(device, queue);
    let compiler = GraphCompiler::new(BuiltinRegistry::default());

    // No audio clips means no audio stream at all. A silent AAC track would
    // be indistinguishable, in the file, from "the mix failed" — better to
    // let the absence be the signal.
    let has_audio = sequence
        .tracks
        .iter()
        .any(|t| t.kind == TrackKind::Audio && !t.clips.is_empty());

    let mut encoder = Encoder::open(
        output,
        width,
        height,
        out_width,
        out_height,
        sequence.settings.frame_rate,
        options,
        has_audio,
    )?;
```

Change the `ExportStats` construction (around lines 299–308) from
`width,` / `height,` to `width: out_width,` / `height: out_height,`:

```rust
    let mut stats = ExportStats {
        frames_written: 0,
        width: out_width,
        height: out_height,
        frames_with_missing_sources: 0,
        audio_written: has_audio,
        loudness: None,
        audio_clips_with_no_source: 0,
        audio_clips_with_unsupported_speed: 0,
    };
```

Now update `Encoder`. Change `Encoder::open`'s signature and body (around
lines 441–509) from:

```rust
    fn open(
        output: &Path,
        width: u32,
        height: u32,
        rate: timeline::FrameRate,
        options: &ExportOptions,
        with_audio: bool,
    ) -> Result<Self, ExportError> {
        let (num, den) = rate.as_rational();
        // FFmpeg time_base is seconds-per-tick, so it's the reciprocal of the
        // frame rate: 30000/1001 fps -> 1001/30000.
        let time_base = ffmpeg_next::Rational::new(den as i32, num as i32);

        let mut octx = ffmpeg_next::format::output(&output.to_path_buf())?;
        let codec =
            ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::H264).ok_or(ExportError::NoGpu)?;
        let mut ost = octx.add_stream(codec)?;
        let mut encoder =
            ffmpeg_next::codec::context::Context::new_with_codec(codec).encoder().video()?;
        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
        encoder.set_time_base(time_base);
        encoder.set_frame_rate(Some(ffmpeg_next::Rational::new(num as i32, den as i32)));

        let mut dict = ffmpeg_next::Dictionary::new();
        let (crf, x264_preset) = options.quality.encoder_settings();
        dict.set("preset", x264_preset);
        dict.set("crf", &crf.to_string());
        let encoder = encoder.open_with(dict)?;
        ost.set_parameters(&encoder);
        ost.set_time_base(time_base);

        let scaler = ffmpeg_next::software::scaling::Context::get(
            ffmpeg_next::format::Pixel::RGBA,
            width,
            height,
            ffmpeg_next::format::Pixel::YUV420P,
            width,
            height,
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )?;
```

to:

```rust
    fn open(
        output: &Path,
        in_width: u32,
        in_height: u32,
        out_width: u32,
        out_height: u32,
        rate: timeline::FrameRate,
        options: &ExportOptions,
        with_audio: bool,
    ) -> Result<Self, ExportError> {
        let (num, den) = rate.as_rational();
        // FFmpeg time_base is seconds-per-tick, so it's the reciprocal of the
        // frame rate: 30000/1001 fps -> 1001/30000.
        let time_base = ffmpeg_next::Rational::new(den as i32, num as i32);

        let mut octx = ffmpeg_next::format::output(&output.to_path_buf())?;
        let codec =
            ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::H264).ok_or(ExportError::NoGpu)?;
        let mut ost = octx.add_stream(codec)?;
        let mut encoder =
            ffmpeg_next::codec::context::Context::new_with_codec(codec).encoder().video()?;
        encoder.set_width(out_width);
        encoder.set_height(out_height);
        encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
        encoder.set_time_base(time_base);
        encoder.set_frame_rate(Some(ffmpeg_next::Rational::new(num as i32, den as i32)));

        let mut dict = ffmpeg_next::Dictionary::new();
        let (crf, x264_preset) = options.quality.encoder_settings();
        dict.set("preset", x264_preset);
        dict.set("crf", &crf.to_string());
        let encoder = encoder.open_with(dict)?;
        ost.set_parameters(&encoder);
        ost.set_time_base(time_base);

        // Source dims match what the compositor actually rendered
        // (`in_width`/`in_height`, always the sequence's native size);
        // destination dims are the (possibly scaled) encoder output. This
        // scaler already existed purely for RGBA->YUV420P pixel-format
        // conversion — giving it different src/dst sizes makes it do the
        // resize in the same pass, so no second scaling step is needed.
        let scaler = ffmpeg_next::software::scaling::Context::get(
            ffmpeg_next::format::Pixel::RGBA,
            in_width,
            in_height,
            ffmpeg_next::format::Pixel::YUV420P,
            out_width,
            out_height,
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )?;
```

Further down in the same function, change the `Ok(Encoder { ... })`
construction (still around line 499–508) from `width, height,` to name
the fields explicitly as the input dims:

```rust
        Ok(Encoder {
            octx,
            encoder,
            scaler,
            width: in_width,
            height: in_height,
            encoder_time_base: time_base,
            stream_time_base,
            audio,
        })
```

(The `width`/`height` fields on the `Encoder` struct itself are
unchanged — still `u32` — only what's passed into them changes. They are
used by `write_frame` to size the *input* RGBA frame, which is why they
must hold the native/input dimensions, not the scaled output ones.)

Finally, update the call site in `export_sequence` you already changed
above — it now passes `width, height, out_width, out_height,` in that
order, matching the new `Encoder::open` signature (this was already
written in the `export_sequence` diff above; no further change needed
here).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p export --test export_sequence exporting_at_half_scale_produces_a_half_sized_file`
Expected: PASS

Run the full export test suite to confirm no regressions:

Run: `cargo test -p export`
Expected: all tests pass, including
`exports_a_real_playable_video_with_the_sequence_dimensions_and_length`
(which uses `ExportOptions::default()`, i.e. `OutputScale::Native`, and
must produce byte-for-byte the same 640x360 output as before this task).

- [ ] **Step 5: Commit**

```bash
git add crates/export/src/lib.rs crates/export/tests/export_sequence.rs
git commit -m "Wire OutputScale into export_sequence's render/encode path"
```

---

## Task 3: Export dialog UI

**Files:**
- Modify: `crates/app/src/main.rs`
  - `ExportUi` struct and its `Default` impl (lines ~717–735)
  - `start_export` (lines ~753–809)
  - File menu block (lines ~1002–1024, inside the `ui.menu_button("File", ...)` closure)
  - New `export_dialog_ui` function, placed directly above `export_ui`
    (before line 815)
  - `build_ui`'s body where `export_ui(ctx, export);` is called (line 1074)

**Interfaces:**
- Consumes: `export::OutputScale` from Task 1/2, `export::QualityPreset`
  (already existed), `start_export` (already existed, gains one new line
  in its `ExportOptions` construction).
- Produces: nothing new consumed by later tasks — this task is
  self-contained.

- [ ] **Step 1: Update `ExportUi`**

In `crates/app/src/main.rs`, change (around lines 717–735):

```rust
struct ExportUi {
    job: Option<export_job::ExportJob>,
    last_result: Option<String>,
    quality: export::QualityPreset,
    /// Export only the marked in/out range. Sticky across exports, since a
    /// range workflow tends to be several exports in a row.
    use_range: bool,
}

impl Default for ExportUi {
    fn default() -> Self {
        ExportUi {
            job: None,
            last_result: None,
            quality: export::QualityPreset::High,
            use_range: false,
        }
    }
}
```

to:

```rust
struct ExportUi {
    job: Option<export_job::ExportJob>,
    last_result: Option<String>,
    quality: export::QualityPreset,
    /// Export only the marked in/out range. Sticky across exports, since a
    /// range workflow tends to be several exports in a row.
    use_range: bool,
    output_scale: export::OutputScale,
    /// Whether the pre-export options window (resolution/quality/range)
    /// is open. Set by the File menu's "Export..." item; cleared either
    /// by the window's own close button or by successfully starting a
    /// job in `export_dialog_ui`.
    show_dialog: bool,
}

impl Default for ExportUi {
    fn default() -> Self {
        ExportUi {
            job: None,
            last_result: None,
            quality: export::QualityPreset::High,
            use_range: false,
            output_scale: export::OutputScale::Native,
            show_dialog: false,
        }
    }
}
```

- [ ] **Step 2: Add `output_scale` to `start_export`'s `ExportOptions`**

In `start_export` (around lines 803–807), change:

```rust
        export::ExportOptions {
            quality: export.quality,
            range_ticks,
            ..Default::default()
        },
```

to:

```rust
        export::ExportOptions {
            quality: export.quality,
            range_ticks,
            output_scale: export.output_scale,
            ..Default::default()
        },
```

- [ ] **Step 3: Replace the File menu's inline controls with a single "Export..." item**

In the `ui.menu_button("File", |ui| { ... })` closure (around lines
993–1024), change:

```rust
                ui.separator();
                if ui
                    .button("Back to Home")
                    .on_hover_text("close this project and return to the project list — unsaved work is still protected by autosave")
                    .clicked()
                {
                    ui.close_menu();
                    *screen = Screen::Home;
                }
                ui.separator();
                ui.label("Export quality:");
                for preset in export::QualityPreset::ALL {
                    ui.radio_value(&mut export.quality, preset, preset.label());
                }
                let has_range = state.marked_range().is_some();
                ui.add_enabled(
                    has_range,
                    egui::Checkbox::new(&mut export.use_range, "Only the in/out range"),
                )
                .on_disabled_hover_text("mark in and out on the timeline first (I and O)");
                if !has_range {
                    // Otherwise an unticked-but-remembered range silently
                    // becomes "whole sequence" with no explanation.
                    export.use_range = false;
                }
                if ui
                    .add_enabled(export.job.is_none(), egui::Button::new("Export Video..."))
                    .clicked()
                {
                    ui.close_menu();
                    start_export(state, export, transport, matting_jobs);
                }
```

to:

```rust
                ui.separator();
                if ui
                    .button("Back to Home")
                    .on_hover_text("close this project and return to the project list — unsaved work is still protected by autosave")
                    .clicked()
                {
                    ui.close_menu();
                    *screen = Screen::Home;
                }
                ui.separator();
                if ui
                    .add_enabled(export.job.is_none(), egui::Button::new("Export..."))
                    .clicked()
                {
                    ui.close_menu();
                    export.show_dialog = true;
                }
```

- [ ] **Step 4: Add the `export_dialog_ui` function**

Add directly above `fn export_ui(ctx: &egui::Context, export: &mut ExportUi) {`
(around line 815) in `crates/app/src/main.rs`:

```rust
/// The pre-export options window: resolution, quality, range. Opened
/// from File > "Export...". Confirming it closes the dialog and hands
/// off to `start_export`, which owns the actual save-file dialog and job
/// creation — unchanged from before this task existed.
fn export_dialog_ui(
    ctx: &egui::Context,
    state: &mut EditorState,
    export: &mut ExportUi,
    transport: &mut Transport,
    matting_jobs: &matting_jobs::MattingJobs,
) {
    if !export.show_dialog {
        return;
    }
    let mut open = true;
    egui::Window::new("Export")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label("Resolution:");
            for scale in export::OutputScale::ALL {
                ui.radio_value(&mut export.output_scale, scale, scale.label());
            }
            ui.separator();
            ui.label("Quality:");
            for preset in export::QualityPreset::ALL {
                ui.radio_value(&mut export.quality, preset, preset.label());
            }
            ui.separator();
            let has_range = state.marked_range().is_some();
            ui.add_enabled(
                has_range,
                egui::Checkbox::new(&mut export.use_range, "Only the in/out range"),
            )
            .on_disabled_hover_text("mark in and out on the timeline first (I and O)");
            if !has_range {
                export.use_range = false;
            }
            ui.separator();
            if ui.button("Export...").clicked() {
                export.show_dialog = false;
                start_export(state, export, transport, matting_jobs);
            }
        });
    if !open {
        export.show_dialog = false;
    }
}
```

- [ ] **Step 5: Call `export_dialog_ui` from `build_ui`**

Find `export_ui(ctx, export);` inside `build_ui` (around line 1074) and
change it to also call the new dialog:

```rust
    export_dialog_ui(ctx, state, export, transport, matting_jobs);
    export_ui(ctx, export);
```

- [ ] **Step 6: Build and fix any compile errors**

Run: `export PATH="$HOME/tools/ffmpeg-n7.1-latest-win64-gpl-shared-7.1/bin:$PATH" && cargo build -p app --bin nle`
Expected: builds clean. If there's a borrow-checker error about `export`
being borrowed both immutably (for `export.job.is_none()` in the File
menu) and mutably elsewhere in the same closure, resolve it the same way
the existing code already does at that call site (read the field into a
local `bool` before the `ui.add_enabled(...)` call if needed).

- [ ] **Step 7: Verify live**

This is pure UI wiring with no new pure logic, so per this project's
testing convention (see Global Constraints), verify by running the app
instead of writing a contrived test:

```bash
powershell -File scripts/deploy-desktop-app.ps1
```

Launch the installed app, open or create a project with a clip on the
timeline, and confirm:
- File menu now shows a single "Export..." item (no inline radios/checkbox).
- Clicking it opens an "Export" window with Resolution (Native/75%/50%/25%),
  Quality (Draft/High/Master), and the range checkbox.
- Picking a resolution other than Native, clicking "Export...", saving to
  a file, and letting the export finish produces a file whose actual
  dimensions (check via `ffprobe` or the export result line, which
  reports `stats.width`x`stats.height`) are scaled down accordingly.
- Native (the default) still produces a file at the sequence's full
  resolution, matching pre-change behavior.

- [ ] **Step 8: Commit**

```bash
git add crates/app/src/main.rs
git commit -m "Add Export dialog with resolution scaling, replacing File-menu inline controls"
```

---

## Task 4: Right-panel tabs

**Files:**
- Modify: `crates/app/src/main.rs`
  - New `RightPanelTab` enum + `RightPanelState` struct, placed near the
    `Screen` enum (around line 49, after it)
  - `main()`'s panel-state setup (around line 121, alongside
    `effects_panel_state`/`scopes_panel_state`/etc.)
  - The right `egui::SidePanel::right("effects")` block (lines 1083–1106)
  - `build_ui`'s signature (around line 930–955) and its call site
    (around line 197–218)

**Interfaces:**
- Produces: nothing consumed by other tasks — self-contained.
- Consumes: `effects_panel::show`, `transcript_panel::show`,
  `scopes_panel::show`, `mixer_panel::show`, `title_panel::show` — all
  unchanged, called exactly as they already are today, just from inside
  a `match` instead of sequentially.

- [ ] **Step 1: Add `RightPanelTab` and `RightPanelState`**

In `crates/app/src/main.rs`, directly after the `Screen` enum (around
line 49):

```rust
/// Which panel is showing in the right-side dock. Previously all four
/// were stacked vertically, which meant scrolling past Effects and
/// Transcript to reach Scopes; this makes only one visible at a time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RightPanelTab {
    Effects,
    Transcript,
    Scopes,
    Mixer,
}

struct RightPanelState {
    tab: RightPanelTab,
}

impl Default for RightPanelState {
    fn default() -> Self {
        RightPanelState { tab: RightPanelTab::Effects }
    }
}
```

- [ ] **Step 2: Instantiate `RightPanelState` in `main()`**

Find `let mut scopes_panel_state = scopes_panel::ScopesPanelState::default();`
(around line 122) and add directly after it:

```rust
    let mut right_panel_state = RightPanelState::default();
```

- [ ] **Step 3: Rewrite the right `SidePanel` block**

Change (around lines 1083–1106):

```rust
    egui::SidePanel::right("effects")
        .resizable(true)
        .default_width(300.0)
        .show(ctx, |ui| {
            // Mixer above effects, collapsed by default: it only matters while
            // you're listening, and strips are wide enough to crowd out the
            // effect controls if it were always open.
            egui::CollapsingHeader::new("Audio Mixer")
                .default_open(false)
                .show(ui, |ui| {
                    let snapshot = transport.audio.as_ref().and_then(|a| a.meters());
                    mixer_panel::show(ui, state, snapshot.as_ref());
                });
            ui.separator();
            // Above the effect list because a title's own text is what you
            // came to the panel for; effects applied *to* the title are the
            // secondary concern. Draws nothing when no title is selected.
            title_panel::show(ui, state);
            effects_panel::show(ui, state, effects_panel_state, matting_jobs);
            ui.separator();
            transcript_panel::show(ui, state, transcript_panel_state);
            ui.separator();
            scopes_panel::show(ui, scopes_panel_state, preview, device, queue_arc);
        });
```

to:

```rust
    egui::SidePanel::right("effects")
        .resizable(true)
        .default_width(300.0)
        .show(ctx, |ui| {
            // Above the tabs because a title's own text is what you came
            // to the panel for; effects applied *to* the title are the
            // secondary concern. Draws nothing when no title is selected.
            title_panel::show(ui, state);
            ui.separator();
            ui.horizontal(|ui| {
                ui.selectable_value(&mut right_panel_state.tab, RightPanelTab::Effects, "Effects");
                ui.selectable_value(&mut right_panel_state.tab, RightPanelTab::Transcript, "Transcript");
                ui.selectable_value(&mut right_panel_state.tab, RightPanelTab::Scopes, "Scopes");
                ui.selectable_value(&mut right_panel_state.tab, RightPanelTab::Mixer, "Mixer");
            });
            ui.separator();
            match right_panel_state.tab {
                RightPanelTab::Effects => {
                    effects_panel::show(ui, state, effects_panel_state, matting_jobs);
                }
                RightPanelTab::Transcript => {
                    transcript_panel::show(ui, state, transcript_panel_state);
                }
                RightPanelTab::Scopes => {
                    scopes_panel::show(ui, scopes_panel_state, preview, device, queue_arc);
                }
                RightPanelTab::Mixer => {
                    let snapshot = transport.audio.as_ref().and_then(|a| a.meters());
                    mixer_panel::show(ui, state, snapshot.as_ref());
                }
            }
        });
```

- [ ] **Step 4: Thread `right_panel_state` through `build_ui`**

In `build_ui`'s signature (around lines 930–955), add a new parameter
right after `scopes_panel_state: &mut scopes_panel::ScopesPanelState,`:

```rust
    right_panel_state: &mut RightPanelState,
```

At `build_ui`'s call site (around lines 197–218), add the matching
argument right after `&mut scopes_panel_state,`:

```rust
                                &mut right_panel_state,
```

- [ ] **Step 5: Build and fix any compile errors**

Run: `export PATH="$HOME/tools/ffmpeg-n7.1-latest-win64-gpl-shared-7.1/bin:$PATH" && cargo build -p app --bin nle`
Expected: builds clean.

- [ ] **Step 6: Verify live**

Pure layout change — verify by running the app (per this project's
testing convention, no contrived unit test):

```bash
powershell -File scripts/deploy-desktop-app.ps1
```

Launch the app, select a clip so Effects has content, then:
- Confirm four tab buttons appear: Effects, Transcript, Scopes, Mixer.
- Click each one and confirm only that panel's content shows — no more
  scrolling past Effects/Transcript to reach Scopes.
- Confirm Effects still works while its tab is active (add an effect,
  adjust a param).
- Confirm Mixer's tab shows the same meters/controls the old collapsible
  header used to.

- [ ] **Step 7: Commit**

```bash
git add crates/app/src/main.rs
git commit -m "Turn the right-side panel stack into tabs (Effects/Transcript/Scopes/Mixer)"
```

---

## Task 5: Drag-and-drop media import

**Files:**
- Modify: `crates/app/src/main.rs`
  - `main()`'s local variable setup (around line 137, alongside `modifiers`)
  - The `match event { ... }` block inside `Event::WindowEvent { event, .. } =>`
    (around lines 153–184, add three new arms)
  - `build_ui`'s signature (around lines 930–955) and its call site
    (around lines 197–218)
  - Top of `build_ui`'s body, after the Home-screen early return (around
    line 976)

**Interfaces:**
- Consumes: `state::EditorState::import_assets(&mut self, paths: Vec<PathBuf>)`
  (already exists, called unchanged — this is the same function
  `project_panel.rs`'s Import button already calls).
- Produces: nothing consumed by other tasks — self-contained.

- [ ] **Step 1: Add the `dragging_file` local**

In `main()`, find `let mut modifiers = winit::keyboard::ModifiersState::empty();`
(around line 137) and add directly after it:

```rust
    // Set by WindowEvent::HoveredFile, cleared by HoveredFileCancelled or
    // DroppedFile — drives a border overlay in build_ui so dropping a
    // file has some visual feedback before it lands.
    let mut dragging_file = false;
```

- [ ] **Step 2: Add the three new `WindowEvent` match arms**

Find the `match event { ... }` block (starts around line 153, right after
`egui_winit_state.on_window_event`). Add three new arms — placement
doesn't matter relative to the existing ones, but grouping them together
after `WindowEvent::ModifiersChanged(new) => modifiers = new.state(),`
(around line 168) keeps related state-setting arms together:

```rust
                    WindowEvent::ModifiersChanged(new) => modifiers = new.state(),
                    WindowEvent::HoveredFile(_) => {
                        dragging_file = true;
                    }
                    WindowEvent::HoveredFileCancelled => {
                        dragging_file = false;
                    }
                    WindowEvent::DroppedFile(path) => {
                        dragging_file = false;
                        // No project is open on the Home screen, so there's
                        // nothing to import into.
                        if screen == Screen::Editor {
                            state.import_assets(vec![path]);
                        }
                    }
                    WindowEvent::KeyboardInput { event: key, .. } => {
```

(This inserts the three new arms between the existing
`ModifiersChanged` and `KeyboardInput` arms — the
`WindowEvent::KeyboardInput { event: key, .. } => {` line shown at the
end is the existing line already there; do not duplicate it.)

- [ ] **Step 3: Thread `dragging_file` through `build_ui`**

In `build_ui`'s signature (around lines 930–955), add a new parameter at
the end, after `recent_projects: &mut recent_projects::RecentProjects,`:

```rust
    dragging_file: bool,
```

At `build_ui`'s call site (around lines 197–218), add the matching
argument at the end, after `&mut recent_projects,`:

```rust
                                dragging_file,
```

- [ ] **Step 4: Draw the overlay**

In `build_ui`'s body, find the Home-screen early return (around lines
959–976):

```rust
    if *screen == Screen::Home {
        egui::CentralPanel::default().show(ctx, |ui| {
            match home_screen::show(ui, recent_projects.entries()) {
                home_screen::HomeAction::None => {}
                home_screen::HomeAction::NewProject => {
                    *state = EditorState::new();
                    *screen = Screen::Editor;
                }
                home_screen::HomeAction::OpenDialog => {
                    open_project(state, recent_projects, screen);
                }
                home_screen::HomeAction::OpenPath(path) => {
                    open_project_path(state, recent_projects, screen, &path);
                }
            }
        });
        return;
    }
```

Add directly after the closing `}` of that block (i.e. right after
`return; }`, before the `egui::TopBottomPanel::top("menu_bar")` line):

```rust

    if dragging_file {
        let screen_rect = ctx.screen_rect();
        ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drag_overlay")))
            .rect_stroke(
                screen_rect.shrink(3.0),
                0.0,
                egui::Stroke::new(4.0, egui::Color32::from_rgb(230, 200, 120)),
            );
    }
```

- [ ] **Step 5: Build and fix any compile errors**

Run: `export PATH="$HOME/tools/ffmpeg-n7.1-latest-win64-gpl-shared-7.1/bin:$PATH" && cargo build -p app --bin nle`
Expected: builds clean.

- [ ] **Step 6: Verify live**

Event wiring around an already-tested function, no new pure logic — verify
live per this project's testing convention:

```bash
powershell -File scripts/deploy-desktop-app.ps1
```

Launch the app, open or create a project, then:
- Drag a video file from Windows Explorer over the app window (don't
  drop yet) — confirm the amber border overlay appears.
- Drop it — confirm the overlay disappears and the file appears in the
  project bin, same as if you'd used Import.
- Drag a file over the Home screen and drop it — confirm nothing happens
  (no crash, no import, since there's no open project).

- [ ] **Step 7: Commit**

```bash
git add crates/app/src/main.rs
git commit -m "Add drag-and-drop media import"
```

---

## Final Step: Full workspace verification

After all five tasks are complete:

- [ ] Run the full workspace test suite:

```bash
export PATH="$HOME/tools/ffmpeg-n7.1-latest-win64-gpl-shared-7.1/bin:$PATH"
cargo test --workspace 2>&1 | grep -E "FAILED|error\[|^error:|test result:"
```

Expected: every `test result:` line reads `0 failed`.

- [ ] Update `docs/decisions-log.md` with an entry covering all three
  changes (tabs, export dialog + scaling, drag-and-drop), following this
  project's existing entry style — what was built, the one real
  implementation subtlety worth recording (the encoder's existing
  RGBA→YUV420P scaler doing double duty as the resize step), and what's
  explicitly out of scope.
