# nle-engine — user manual

Everything the editor does today, in the order you'd meet it: bring footage in,
cut it, treat it, and get a file out the other end.

Facts here are read from the code rather than assumed; when a behaviour looks
surprising, the reason it works that way is given. A web version of this
manual exists as an Artifact — this file is the copy that travels with the
repository.

| | |
|---|---|
| Timebase | 254,016,000,000 ticks/second |
| Autosave | every 20 seconds, only when something changed |
| Undo depth | 100 steps, persisted in the project file |
| Project files | `.nleproj` |

---

## Starting a project

The home screen is the first thing you see, and the only screen that isn't the
editor.

**New Project** opens an empty sequence called Sequence 01 at 1920x1080,
30 fps, 48 kHz. You don't have to keep those numbers — the first clip you drop
on an empty timeline resizes the sequence to match itself, so a 640x360 phone
clip won't sit as a small island in a large black frame. This only applies
while the timeline is still empty, so it can never re-frame an edit in
progress.

**Open...** takes a `.nleproj` file. Projects you've opened or saved appear
under Recent Projects.

If the last session ended without saving, you're offered the recovery file
before anything else — see [Saving and recovery](#saving-and-recovery).

A project file carries its own undo history, so reopening a project doesn't
cost you the ability to step back through the work in it.

---

## Bringing media in

**Import** in the Project panel opens a file picker. You can also drag files
straight onto the window — a multi-file drop is one undo step, not one per
file.

Readable containers: MP4/QuickTime, Matroska and WebM, AVI, MPEG-TS, ASF/WMV,
FLV, and the common audio wrappers — WAV, MP3, FLAC, Ogg, AAC. Anything else
is refused at import with the reason in the status line. This list is a
deliberate allowlist, not a limitation of FFmpeg: see `docs/security.md`.

Frame sizes are capped at 8192x8192 (above 8K DCI). A file declaring something
larger is refused rather than allowed to decide how much memory the editor
asks for.

### Bins

**New Bin** makes a folder. Drag assets and sequences into it, right-click to
rename. **Delete bin (keeps contents)** removes the folder and promotes
whatever was inside it to the parent — deleting a folder never takes the
footage with it.

### Proxies

Heavy footage scrubs badly because most delivery codecs need decoding forward
from the last keyframe. A proxy is a small all-intra copy — every frame a
keyframe — capped at 960 px on its long edge. Two wins, and the second is the
bigger one: fewer pixels to decode, and no inter-frame dependencies, so
seeking doesn't have to decode forward from a keyframe.

**Build proxies** transcodes everything imported; **Build proxy** on a clip's
right-click menu does just that one. Tick **Use proxies** to actually play from
them. The column beside each asset reads `...` while building, `yes` when
ready, `err` if the transcode failed. A failed asset is not retried
automatically, so an unreadable file doesn't spawn a fresh doomed transcode
every time the panel is drawn.

**Export never uses proxies.** They are for playback, preview and scrubbing
only. Quietly delivering a 960 px render as the finished product would be the
worst possible outcome of the feature, so export always reads the original
paths.

---

## The window

| Area | Where | What's in it |
|---|---|---|
| Project | left | Assets, sequences and bins; search; proxy column; Import and New Bin |
| Preview | centre top | The frame under the playhead |
| Timeline | centre bottom | Tracks, clips, playhead, toolbar |
| Right panel | right | Four tabs — Effects, Transcript, Scopes, Mixer |

The status line sits in the timeline's toolbar row and reports the last thing
that happened: what an edit refused to do and why, what an analysis found,
whether a save succeeded.

---

## Your first edit

1. **Import the footage.** Project panel -> Import, or drag the file onto the
   window.
2. **Put it on the timeline.** Select the asset and press **Add to Timeline**.
   A file with both picture and sound lands as a linked pair: video on V1,
   audio on A1.
3. **Find your cut.** `Space` plays, arrow keys walk frame by frame, `Up` and
   `Down` jump between existing edit points.
4. **Cut.** Press `C` for the razor and click the clip where you want the cut.
   `V` returns to the select tool.
5. **Remove what you don't want.** Click the unwanted piece and press
   `Shift+Delete` to ripple it out, closing the gap. Plain `Delete` leaves the
   gap instead.
6. **Tidy the join.** Drag a clip's edge to trim, or right-click the cut for a
   transition.
7. **Export.** The Export button opens the dialog.

---

## Playback and navigation

`Space` toggles play and stop. `K` stops. `L` plays forward and `J` plays
backward; press either again and the speed doubles — 1x, 2x, 4x, up to 8x.
Pressing the opposite key snaps straight back to 1x in that direction rather
than stepping down through the speeds.

**Shuttle is silent.** Only normal 1x playback runs the full audio-and-video
path; any other speed plays picture without sound.

`Left` and `Right` step one frame, `Shift` for five. `Up` and `Down` jump to
the previous or next edit point — clip starts and ends, your in and out marks,
and the start of the sequence. `Home` and `End` go to the beginning and end.

If the picture stutters, the toolbar says whether frames are being dropped.
That distinguishes "this machine can't keep up" from "the footage itself is
choppy", which decides whether the answer is proxies or a re-export.

---

## Cutting and trimming

### Tools

`V` selects the **Select** tool — click to select, drag to move a clip along
its track or onto another, drag an edge to trim. `C` selects the **Razor** —
click anywhere on a clip to cut it there.

### Lift versus ripple

`Delete` **lifts**: the clip goes, the gap stays, everything downstream keeps
its timing. `Shift+Delete` **ripple deletes**: the gap closes and everything
after it slides earlier. The distinction matters enough that guessing one
would be wrong half the time, so the editor doesn't guess.

`Ctrl+X` is copy-then-ripple-delete. `Ctrl+V` pastes at the playhead. The
clipboard holds whole clips — effects, keyframes, gain and all — so a paste
still works after the clip you copied from has been deleted.

### Snapping and marks

`S` toggles snapping, on by default. When you drag a clip, whichever of its
two edges lands closer to a snap point wins — snapping the head only would
make it impossible to butt a clip's *tail* against the next clip, which is
half of what snapping is for.

`I` and `O` set the in and out marks at the playhead. Setting an in past the
out clears the out rather than leaving an inverted range every range operation
would then have to special-case. **Clear marks** appears in the toolbar once
either is set. Opening a different project clears them, since marks are
positions in a sequence and mean nothing in a different one.

### Sync lock

Tracks are sync-locked by default, so a ripple on one track shifts the others
with it and audio stays under picture. Locking a track outright makes edits on
it refuse rather than proceed.

---

## Transitions

Right-click a clip near the edge you want and choose **Cross Dissolve**, **Dip
to Black**, **Wipe** or **Slide**. **Remove** takes one off.

A transition is centred on the cut and reads into both clips' handles — the
unused footage beyond their trim points. Its length is clamped to what the
shorter side can supply, and the status line says when it was shortened for
that reason. One transition per cut: asking again replaces rather than
stacking two that would both claim the same ticks.

At the head or tail of a track, where there's only one clip to work with, a
transition becomes a fade in or out instead.

Transitions ride along with ripple edits, so deleting a clip earlier on the
track doesn't leave a dissolve sitting on somebody else's cut.

---

## Titles

**Add Title** inserts a text clip at the playhead on the topmost video track,
or on a new track above if that one is occupied — it never overwrites existing
material. A title is a clip rather than an effect, so editing its text is an
ordinary project change and undoes like any other.

---

## Effects

Select a clip, then the Effects tab. **Add Effect** offers six, applied in the
order you stack them:

| Effect | What it does |
|---|---|
| Transform | Position, scale, rotation |
| Gaussian Blur | Separable two-pass blur; radius zero is a true no-op |
| Color Correction | Exposure, contrast, saturation, temperature, tint — one effect with five controls, the way a grading tool works |
| Crop | Trims the frame's edges |
| Mask | Rectangle or ellipse with feather, evaluated in the clip's own space so it travels with the clip when transformed |
| Chroma Key | Keys out a colour |

Each effect can be disabled without removing it, so you can compare with and
without.

---

## Keyframes

Any numeric effect control can animate. Keyframes are clip-relative, so moving
a clip carries its animation with it rather than leaving the motion behind on
the timeline.

Three interpolation modes: **Hold** keeps the value until the next keyframe,
**Linear** runs straight between them, **Bezier** eases. Bezier handles can be
dragged; left alone they're computed automatically from the neighbouring
keyframes. The arrows beside a parameter jump to the previous and next
keyframe.

Dragging a keyframe or a handle is one undo step, however many frames the drag
passed through.

---

## Automatic tools

At the top of the Effects tab, acting on the selected clip. Each runs in the
background: the button becomes a spinner naming what it's doing, and the rest
of the editor stays usable. Different actions can run at once; the same action
twice on one clip is refused while it's still going.

| Action | What it does |
|---|---|
| Detect Scene Cuts | Finds hard cuts and razors the clip at each one |
| Remove Silence | Finds silent gaps and ripple-deletes them |
| Detect Beats | Places a marker on each onset, for cutting to music |
| Match Loudness | Measures the clip and applies the gain that reaches your target — default -14 LUFS |
| Stabilize | Analyses motion and writes position keyframes countering it |
| Generate Captions | Transcribes speech and places one title clip per phrase |
| Remove Background | AI matting with a local model; nothing leaves the machine |

**If you edit while one runs**, the result is only applied if the clip is still
where it was — same track, same media, same trim, same position, same speed.
Move or re-time the clip mid-analysis and the result is discarded with a note
saying so, rather than cutting in the wrong place. Run it again afterwards.

**Expect minutes, not seconds.** Scene-cut detection decodes across the whole
clip, and on sparse-keyframe footage — screen recordings especially — that can
run well past ten minutes. There is no progress percentage yet, so a spinner
that looks stuck is usually just working.

Everything except Match Loudness needs a clip at constant speed and refuses
with a reason if the speed is keyframed.

---

## The transcript

Select a clip, open the **Transcript** tab, press **Transcribe**. Transcription
is local Whisper — fully offline. Generating captions transcribes anyway, so
doing both doesn't run it twice.

Click a word to jump the playhead to it. Click one, then `Shift`-click another
to select a run. **Delete N words** ripple-deletes the footage those words
occupy.

A selection touching either end of the clip is refused: trimming an edge and
isolating a passage from the middle are different operations, and only the
second is built.

A transcript goes **out of date** when the clip is moved, trimmed or re-timed,
because the word timings were measured against where the clip used to sit. The
panel says so and asks you to transcribe again, rather than letting you cut by
numbers that no longer line up.

---

## Audio and the mixer

The **Mixer** tab lists audio tracks. Each has a fader in dB, a pan control,
and mute and solo. Solo is exclusive: if any track is soloed, tracks that
aren't go silent. Mute wins over solo on the same track, matching every NLE.

The master fader is monitoring only and is deliberately **not** saved into the
project, so a quiet monitoring choice can't follow the file into an export.

Clip gain is separate from track gain, lives on the clip, and travels with a
copy and paste. Both can be keyframed.

If the sound card is missing or in use, playback falls back to the wall clock
and says so in the status line — picture still runs at the right speed, there
is just no sound.

---

## Scopes

The **Scopes** tab shows an R/G/B histogram on a log height scale, a luma
waveform, and a vectorscope for hue and saturation. They read the frame under
the playhead, so scrub to what you want to judge.

Export also measures loudness as it renders, reporting integrated LUFS,
true-peak dBTP and loudness range when it finishes.

---

## Exporting

**Resolution** is Native, 75%, 50% or 25% of the sequence size. **Quality** is
Draft (fast, larger), High (recommended), or Master (near-lossless, slow).

**Only the in/out range** becomes available once in and out are marked. A range
export starts its timestamps at zero, so the file behaves like a clip in its
own right rather than one carrying an offset.

Progress appears in its own window with a Cancel button, because an export
takes long enough that "did I actually start it?" is a real question. A second
export won't start while one is running — they would contend for the same
encoder and just make each other slower.

Export always reads your original media. A clip whose file has gone missing
renders as a gap rather than failing the whole export.

---

## Saving and recovery

**Save** writes to the project's own file, or falls through to **Save As...**
if it has never been saved. Writes are atomic — temporary file, flush, rename
— so an interrupted save can't leave half a project.

Media is referenced by a path relative to the project folder where possible
and an absolute path otherwise, plus a content hash, so a self-contained
project folder can be moved or copied wholesale and still find its footage.

A project is checked when it loads: no overlapping clips, no clip ending
before it starts, no duplicate ids, a usable sample rate and frame size. A
file failing those checks is refused rather than opened into a state where
every subsequent edit would fail.

### Autosave

Every **20 seconds**, and only when something has actually changed, a separate
recovery file is written — beside the project as `.recover-yourproject.nleproj`,
or in the temp folder for a project that has never been saved. Saving or
quitting cleanly deletes it, so the next launch only offers recovery when
there is genuinely something to recover. Each editor session gets its own
recovery file, so two windows can't overwrite each other's unsaved work.

Recovery files never become the project. Adopting one would mean the next
`Ctrl+S` silently wrote over the recovery file instead of your actual work.

Undo history is **not** carried into the recovery file: a recovery write
happens every few seconds, and serialising the whole undo stack each time
would make autosave cost more the longer you worked. A crash therefore costs
the history, not the work.

---

## Keyboard reference

### Transport

| Key | Action |
|---|---|
| `Space` | Play / stop |
| `L` | Play forward — again to double, to 8x |
| `J` | Play backward — again to double, to 8x |
| `K` | Stop |

### Navigation

| Key | Action |
|---|---|
| `Left` / `Right` | Step one frame |
| `Shift+Left` / `Right` | Step five frames |
| `Up` / `Down` | Previous / next edit point |
| `Home` / `End` | Start / end of sequence |

### Editing

| Key | Action |
|---|---|
| `V` | Select tool |
| `C` | Razor tool |
| `S` | Toggle snapping |
| `I` / `O` | Mark in / out at the playhead |
| `Delete` | Lift — remove, leave the gap |
| `Shift+Delete` | Ripple delete — remove, close the gap |
| `Ctrl+C` | Copy selection |
| `Ctrl+X` | Cut — copy, then ripple delete |
| `Ctrl+V` | Paste at the playhead |
| `Ctrl+A` | Select every clip |
| `Esc` | Clear the selection |

### History

| Key | Action |
|---|---|
| `Ctrl+Z` | Undo |
| `Ctrl+Shift+Z` | Redo |
| `Ctrl+Y` | Redo |

`V` and `C` are the select and razor tools, as in Premiere, so copy and cut
need `Ctrl` — which leaves the bare keys free for the tools you reach for
constantly.

Holding a key repeats navigation but never repeats an edit: an autorepeated
delete or paste would fire dozens of times from one keypress.

---

## What isn't built yet

Real gaps, listed so you don't go looking for them.

- **Speed and retiming have no controls.** Slip, slide and rate stretch exist
  in `timeline::edit_ops` and are covered by tests, but nothing in the
  interface reaches them.
- **Track management is minimal.** One video and one audio track are created
  on first use; there is no insert-track UI, and mute and solo are offered for
  audio tracks only.
- **No progress percentage on analysis.** Long jobs show a spinner and no
  estimate.
- **Missing media can't be relinked.** A project whose footage has moved opens
  with those clips as gaps. Relinking by content hash is designed, not written.
- **The colour toolset is a slice.** Curves, vibrance, highlight/shadow
  controls, HSL secondaries and LUT loading are not implemented.
- **Nothing is sandboxed.** Media is parsed in-process. Containers are
  restricted and frame sizes capped, but a decoder bug is still a bug inside
  the editor. See `docs/security.md`.
