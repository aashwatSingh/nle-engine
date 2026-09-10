// rustup's bundled self-contained GNU linker doesn't ship every Windows
// import lib — specifically libshlwapi.a, pulled in transitively by
// egui-winit's clipboard support via `arboard`/`clipboard-win`.
//
// `vendor-lib/` holds *only* that one file (copied from the separately
// installed mingw64 toolchain's lib dir — see .cargo/config.toml), not the
// whole directory. Two earlier, broader attempts both corrupted the
// resulting binary/toolchain:
//   1. A workspace-wide `rustflags` entry adding that whole lib dir to
//      every target's search path — including build-script host
//      binaries — caused STATUS_STACK_OVERFLOW in totally unrelated
//      crates' build scripts (quote, proc-macro2, serde_core, ...).
//   2. Scoping the whole directory to just this crate (via
//      `cargo:rustc-link-search` here) still crashed this crate's own
//      test harness binary with the same error, even though the normal
//      `nle` binary linked from it ran fine.
// Both are consistent with the same root cause: that directory has other
// same-named libs (mingwex, msvcrt, pthread, ...) from a different
// mingw-w64 generation than rustup's self-contained one, and the linker
// picking those instead of the self-contained toolchain's own versions
// produces an ABI-mismatched binary. Shipping only the one missing file
// removes that ambiguity — nothing else in this directory can shadow
// anything the self-contained toolchain already provides.
//
// Its provenance is proven, not just recorded: it's byte-identical to
// `x86_64-w64-mingw32/lib/libshlwapi.a` in winlibs' MinGW-w64 build (GCC
// 16.1.0 r4, by Brecht Sanders) — the toolchain it was copied from.
const LIBSHLWAPI_SHA256: &str = "7ebcf0a50d860377af1fa308a541cadbecd0d79bd481969f635a67f4f44e1fb2";

fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // Pinned because it's linked straight into nle.exe. An import library
    // should hold only stubs, but nothing stops a swapped one from carrying
    // real object code, and nothing else in the build would notice. See
    // docs/security.md.
    let vendored = manifest_dir.join("vendor-lib").join("libshlwapi.a");
    if let Err(e) = integrity::verify(&integrity::Pin::new(&vendored, LIBSHLWAPI_SHA256)) {
        panic!("{e}");
    }
    println!("cargo:rustc-link-search=native={}", manifest_dir.join("vendor-lib").display());

    // Embeds assets/icon.ico into the .exe (Explorer, taskbar, Alt+Tab, and
    // any shortcut that doesn't override its own icon all pick it up from
    // here) via `embed-resource`, which drives `windres` rather than
    // assuming MSVC's `rc.exe` — this project builds with the GNU toolchain
    // throughout (see docs/decisions-log.md and this crate's FFmpeg linking
    // above), so an MSVC-only resource compiler would silently do nothing on
    // this target.
    //
    // `windres` isn't the mingw64 install rustup's self-contained GNU
    // toolchain uses internally, and isn't on cargo's build-script PATH by
    // default — same reason `.cargo/config.toml` sets `LIBCLANG_PATH` and
    // `BINDGEN_EXTRA_CLANG_ARGS` at the *portable* mingw64 install this
    // machine already has for FFmpeg's bindgen step, not Git's own bundled
    // one. Prepending it here (scoped to this build-script process only, not
    // a global rustflags change) is the same, already-proven-safe pattern
    // the vendor-lib link-search above uses — see that comment for why a
    // *global* PATH/link-search change previously corrupted unrelated
    // builds, which a build-script-local env var can't do.
    let mingw_bin = integrity::tools_dir().join(r"mingw64\bin");
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{};{path}", mingw_bin.display()));
    embed_resource::compile("assets/icon.rc", embed_resource::NONE);
}
