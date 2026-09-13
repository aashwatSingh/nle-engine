# Security notes

What this editor trusts, what checks that trust, and what is still open.
Written 2026-09-10; update it when any of the facts below change.

## The threat model, briefly

A single-user Windows desktop app. It opens media files the user chooses, and it
loads third-party native code — FFmpeg, ONNX Runtime, whisper.cpp — and model
files into its own process or a child process. Two risks are realistic:

1. **A hostile or malformed media file** reaching a parser bug.
2. **A replaced binary or model**: a tampered or corrupted re-download, a bad
   copy, or another program writing into the tools directory.

What is *not* defended against: someone who can already write to this user's
profile as this user. They could replace `nle.exe` itself, and no check inside
it would help. The pins below are about getting exactly the files that were
vetted, not about resisting a compromised account.

## Pinned third-party files

Every file below is checked against a SHA-256 hash before it is used, and a
mismatch refuses to load rather than warning.

| Component | Files | Pinned in | Vetted build |
|---|---|---|---|
| ONNX Runtime 1.29.0 | `onnxruntime.dll`, `onnxruntime_providers_shared.dll` | `crates/matting/src/lib.rs` | Microsoft's `onnxruntime-win-x64-1.29.0.zip` (zip SHA-256 `c9b4b708…`) |
| RVM background-removal model | `rvm_mobilenetv3_fp32.onnx` | `crates/matting/src/lib.rs` | downloaded 2026-08-16 |
| whisper.cpp | `whisper-cli.exe`, `whisper.dll`, `ggml.dll`, `ggml-base.dll`, all nine `ggml-cpu-*.dll` | `crates/speech/src/whisper.rs` | `whisper-bin-x64.zip` (zip SHA-256 `49dcc16d…`), downloaded 2026-08-14 |
| Whisper model | `ggml-base.en.bin` | `crates/speech/src/whisper.rs` | downloaded 2026-08-14 |
| FFmpeg | the eight `av*`/`sw*`/`postproc` DLLs | `scripts/deploy-desktop-app.ps1` | BtbN `ffmpeg-n7.1-latest-win64-gpl-shared-7.1`, FFmpeg n7.1.5-12-g1fdbca85aa (2026-08-07) |

All nine `ggml-cpu-*.dll` files are pinned, not just the one this machine uses,
because `whisper-cli.exe` picks one at startup to match the CPU.

### How the check works

The `integrity` crate hashes each file through a handle that lets other
processes read the file but not write, rename or delete it, and hands that
handle back. The caller keeps it open until the load has finished, so a file
can't be swapped between being checked and being used. That this doesn't get
in the loader's way was measured, not assumed: with the handles held, the real
`whisper-cli.exe` ran and transcribed, and the real ONNX Runtime loaded and ran
the real model.

- **Background removal** holds the model and both runtime DLLs until the ONNX
  session exists.
- **Transcription** holds the CLI, its DLLs and the model until `whisper-cli`
  exits.
- **FFmpeg** can't be checked by the app: `nle.exe` imports its DLLs, so they
  load before any of the app's code runs. The deploy script checks them twice:
  at the source before the install directory is touched, and again on the
  copies, since the copies are what the app loads.

### What pinning doesn't cover

- **Files that shouldn't be there at all.** Windows searches an executable's own
  directory first, so a DLL dropped beside `whisper-cli.exe` under a system
  DLL's name (`MSVCP140.dll`, say) would load without being pinned. Pins check
  known files; they don't prove that no unknown ones exist.
- **FFmpeg in a dev shell** comes from `PATH` and is unchecked. Only the
  deployed copy is pinned.
- **Windows' own DLLs**, including the Visual C++ runtime.

A related bug was fixed along the way. Before `ort` had been pointed at the
pinned DLL, turning an error into `ort::Error` made `ort` fall back to
`LoadLibrary("onnxruntime.dll")` by bare name. On this machine that found an
unrelated 1.17.1 copy on the DLL search path and panicked on the version
mismatch. That also meant a missing runtime crashed the matting worker instead
of reporting an error. `RvmSession::load` now has its own error type, and it
touches no `ort` API until the pinned DLL is verified and loaded.

## Re-pinning after a deliberate upgrade

1. Download from the official source. Where the publisher lists a checksum,
   compare against it; whisper.cpp's GitHub release assets carry one.
2. Hash every file you'll pin:
   `Get-FileHash -Algorithm SHA256 <file>`
3. Update the constants in the file named in the table, including the version
   notes beside them.
4. Run the tests that load the real files:
   ```
   cargo test -p integrity -p matting -p speech
   cargo test -p matting -- --ignored
   NLE_REAL_FOOTAGE_PATH=<a real recording> cargo test -p speech --test whisper_real_footage -- --ignored
   ```
   For FFmpeg, run `scripts/deploy-desktop-app.ps1`; it refuses to install a
   DLL that doesn't match.
5. Record the upgrade in `docs/decisions-log.md`.

A pin that fails *without* a deliberate upgrade means the file changed on its
own. Treat that as a problem to investigate, not a hash to update.

## Untrusted media

FFmpeg parses imported files in-process with no sandbox. That was a deliberate
early trade (see `docs/decisions-log.md`, 2026-08-07, "In-process FFmpeg FFI for
M1, not sandboxed decode"). It means a memory-safety bug in an FFmpeg parser is
code execution inside the editor.

**What limits it now.** Every open in `media_ffmpeg` goes through `open_input`,
which sets two FFmpeg options:

- `format_whitelist` allows only the demuxers import can need: MP4/QuickTime
  (also this app's own proxies, mattes and exports), Matroska/WebM, AVI,
  MPEG-TS, ASF/WMV, FLV, WAV, MP3, FLAC, Ogg and AAC. FFmpeg chooses a demuxer
  by content, not by extension, so without this a file named `.mp4` could
  select any of several hundred — including ones that go on to open other
  files or URLs. A test checks that a concat script named `holiday.mp4` is
  refused. Before the allowlist it opened, and FFmpeg followed it to the file
  it named.
- `protocol_whitelist=file` keeps even an allowed demuxer from reaching past
  local files.

Frame dimensions are bounded too (`MAX_DECODED_PIXELS`, 8192x8192 — above 8K
DCI). They come from the file's own headers and size a buffer allocated per
frame at four bytes a pixel, so without a cap the file decides how much memory
the process asks for; FFmpeg's own default (`max_pixels` = `INT_MAX`) is far
too generous for that. At the top of that range the arithmetic also stopped
being merely large and started being wrong — `width * height * 4` was `u32`,
which wraps in release builds, and a wrapped length allocated a buffer too
small for the copy that followed. It is computed in `u64` now and checked
before the decoder is used.

**What that doesn't cover.** Decoders aren't allowlisted; the allowlist bounds
containers, not codecs. The allowed demuxers are themselves large parsers. A
real sandbox — decoding in a separate low-privilege process — is the only
complete answer, and it isn't built.

## Untrusted project files

A `.nleproj` is untrusted input in the same way media is: it can arrive by
download or email, and nothing about opening one implies the user vouched for
its contents.

- **Structure is validated on load.** `timeline::model::invariants::check_project`
  holds an arriving project to the same standard `edit_ops::apply` holds its
  own results to — no overlapping clips, no clip ending before it starts, no
  duplicate clip/track/sequence ids, no negative start, a usable sample rate
  and frame size. `apply` re-checks the *whole* project after every edit, so a
  file skipping that check didn't merely load something wrong, it loaded
  something that made every later edit fail while blaming the edit. Storage
  order is normalised rather than rejected, because `apply` sorts before it
  checks and an unsorted vec is not corruption. Undo history is held to the
  same standard but dropped rather than refused — it's recoverable context,
  not the work itself.
- **Media paths can't escape the project folder.** `relative_path` exists so a
  self-contained project folder can be moved wholesale, and `Path::join`
  silently discards the base when handed an absolute path — so the field
  marked "relative" could otherwise name any file on the machine and the
  editor would try to decode it. `resolve_relative_media` refuses anything
  absolute or climbing through `..`. Media genuinely stored elsewhere still
  resolves through `original_absolute_path`, which is what that field is for.

**What that doesn't cover.** `original_absolute_path` is by design an
arbitrary path, so a crafted project can still name any file and have the
editor attempt to decode it. The demuxer allowlist bounds what that attempt
can do — no allowed demuxer can reference further files or URLs, and there is
no network path out of this process — so the residual is file-existence
probing and a decode attempt that almost always fails.

### Update status (checked 2026-09-10)

- **FFmpeg.** The installed 7.1.5 is the newest 7.1 point release (upstream,
  2026-06-20). Upstream has also released 8.1.2 and 9.0.1. BtbN's automated
  builds no longer include the 7.1 branch, so a future 7.1 fix won't arrive
  from the build source this project uses. Moving to 8.1 means `ffmpeg-next` 8
  and re-checking this crate against the new API. Not done.
- **ONNX Runtime.** 1.29.0 installed; 1.29.1 is available, a patch release that
  lists no security fixes.
- **whisper.cpp.** The installed build predates the current release (`b4938`,
  2026-08-20). `whisper-cli` only ever reads the WAV this app writes and the
  pinned model, so its input isn't attacker-shaped. The exposure is the binary
  itself, which is pinned.

## Build-time inputs

- **`ort`'s default features are off.** They added `download-binaries` and
  `tls-native` — a build-time HTTP client and native TLS stack for fetching an
  ONNX Runtime this project never uses, since it loads the pinned DLL itself —
  plus `copy-dylibs`, `ndarray` and `tracing`, none of which the code touches.
  Only `load-dynamic` and `api-27` remain.
- **`crates/app/vendor-lib/libshlwapi.a` is pinned.** It's the one prebuilt
  binary committed to the repository, and it's linked straight into `nle.exe`.
  Its provenance is confirmed by hash, not just by comment: it's byte-identical
  to `x86_64-w64-mingw32/lib/libshlwapi.a` in winlibs' MinGW-w64 build (GCC
  16.1.0 r4, by Brecht Sanders), the toolchain `build.rs` says it came from.
  `build.rs` refuses to build if it changes.

## Machine paths and the account name

Source used to hardcode `C:\Users\<account>\tools\…`, which published the local
account name in a public repository. Code now finds tools through
`integrity::tools_dir()`: `NLE_TOOLS_DIR` if it's set, otherwise
`%USERPROFILE%\tools`. The deploy script and `build.rs` follow the same rule.
`.cargo/config.toml` can't expand environment variables, so it's now generated
per machine: `scripts/configure-machine.ps1` fills in
`.cargo/config.toml.example`, and the output is gitignored.

This stops new exposure only. Earlier commits still contain the old paths.
Removing them would mean rewriting published history, which breaks every
existing clone — a decision for the repository's owner, not a cleanup step.
