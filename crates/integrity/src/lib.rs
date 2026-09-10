//! SHA-256 pinning for the native code and model files this project loads
//! from outside its own build: ONNX Runtime and the RVM model for background
//! removal, whisper.cpp's CLI, DLLs and model for speech.
//!
//! Every one of those runs with this process's full privileges — a DLL runs
//! arbitrary code the moment it's mapped, and the model files are fed to
//! native parsers. They were downloaded once by hand and then trusted by path
//! alone, so a replaced file would have been loaded without question. A pin is
//! the hash of the file as it was vetted; anything else is refused.
//!
//! Checking a hash and then loading by path leaves a gap: the file could be
//! swapped in between. On Windows `verify` closes it by hashing through a
//! handle that denies writes, renames and deletes, and handing that handle
//! back. While the `Verified` lives, the file can still be read, executed and
//! loaded — measured, not assumed: a held `whisper-cli.exe` still ran and a
//! held `onnxruntime.dll` still loaded — but it can't be replaced. Keep it
//! alive until the load has finished.

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

/// A file, and the SHA-256 its contents must have (hex, either case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub path: PathBuf,
    pub sha256: &'static str,
}

impl Pin {
    pub fn new(path: impl Into<PathBuf>, sha256: &'static str) -> Self {
        Pin { path: path.into(), sha256 }
    }
}

/// A pinned file that matched — and, on Windows, the lock that keeps it
/// matching until this is dropped.
#[derive(Debug)]
pub struct Verified {
    path: PathBuf,
    _handle: File,
}

impl Verified {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug)]
pub enum IntegrityError {
    /// Missing, or couldn't be opened or read.
    Unreadable { path: PathBuf, source: std::io::Error },
    /// Readable, but not the file that was vetted.
    Mismatch { path: PathBuf, expected: String, actual: String },
}

impl fmt::Display for IntegrityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntegrityError::Unreadable { path, source } => {
                write!(f, "can't verify {}: {source}", path.display())
            }
            IntegrityError::Mismatch { path, expected, actual } => write!(
                f,
                "{} failed its integrity check (expected SHA-256 {expected}, found {actual}) — \
                 it has changed since it was vetted; reinstall it, or re-pin it deliberately \
                 (see docs/security.md)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for IntegrityError {}

/// Hashes `pin.path` and returns it held open if it matches.
pub fn verify(pin: &Pin) -> Result<Verified, IntegrityError> {
    let unreadable = |source| IntegrityError::Unreadable { path: pin.path.clone(), source };
    // Hash through the same handle that holds the lock, so what was checked
    // is exactly what can't be swapped afterwards.
    let mut handle = open_locked(&pin.path).map_err(unreadable)?;
    let actual = sha256_hex(&mut handle).map_err(unreadable)?;
    let expected = pin.sha256.to_ascii_lowercase();
    if actual != expected {
        return Err(IntegrityError::Mismatch { path: pin.path.clone(), expected, actual });
    }
    Ok(Verified { path: pin.path.clone(), _handle: handle })
}

#[cfg(windows)]
fn open_locked(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // `FILE_SHARE_READ` alone: others may still read the file — which is all
    // executing or loading it needs — but not write, rename or delete it
    // while this handle is open.
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(path)
}

#[cfg(not(windows))]
fn open_locked(path: &Path) -> std::io::Result<File> {
    // No share modes here: the hash is still checked, but nothing stops a
    // swap afterwards.
    File::open(path)
}

fn sha256_hex(file: &mut File) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    std::io::copy(file, &mut hasher)?;
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `verify` for each pin, in order, stopping at the first failure.
pub fn verify_all(pins: &[Pin]) -> Result<Vec<Verified>, IntegrityError> {
    pins.iter().map(verify).collect()
}

/// Where this machine's hand-installed third-party tools live: `NLE_TOOLS_DIR`
/// if it's set, otherwise `tools` in the user's profile. Worked out at runtime
/// rather than written into the source, which published one machine's account
/// name in every copy of the code. This only says where to look — the pins are
/// what make a file found there trustworthy, wherever that is.
pub fn tools_dir() -> PathBuf {
    tools_dir_from(std::env::var_os("NLE_TOOLS_DIR"), std::env::var_os("USERPROFILE"))
}

fn tools_dir_from(override_dir: Option<std::ffi::OsString>, profile: Option<std::ffi::OsString>) -> PathBuf {
    match override_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(profile.unwrap_or_default()).join("tools"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-2's own test vector for "abc".
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const ABC_UPPER: &str = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";

    fn file_with(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_file_matching_its_pin_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_with(dir.path(), "model.bin", b"abc");

        let verified = verify(&Pin::new(&path, ABC)).expect("the standard test vector should match");

        assert_eq!(verified.path(), path);
    }

    #[test]
    fn a_pin_written_in_uppercase_hex_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_with(dir.path(), "model.bin", b"abc");

        assert!(verify(&Pin::new(&path, ABC_UPPER)).is_ok());
    }

    #[test]
    fn a_changed_file_is_refused_and_the_error_names_both_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_with(dir.path(), "onnxruntime.dll", b"abd");

        let err = verify(&Pin::new(&path, ABC)).expect_err("one changed byte must be refused");

        match &err {
            IntegrityError::Mismatch { path: p, expected, actual } => {
                assert_eq!(p, &path);
                assert_eq!(expected, ABC);
                assert_eq!(actual.len(), 64, "actual should be a full hex digest: {actual}");
                assert_ne!(actual, ABC);
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains("onnxruntime.dll") && message.contains(ABC), "{message}");
    }

    #[test]
    fn a_missing_file_is_unreadable_rather_than_a_mismatch() {
        let dir = tempfile::tempdir().unwrap();

        let err = verify(&Pin::new(dir.path().join("absent.dll"), ABC)).unwrap_err();

        assert!(matches!(err, IntegrityError::Unreadable { .. }), "{err:?}");
    }

    #[test]
    fn verify_all_reports_the_first_file_that_fails() {
        let dir = tempfile::tempdir().unwrap();
        let good = file_with(dir.path(), "good.dll", b"abc");
        let bad = file_with(dir.path(), "bad.dll", b"tampered");

        let err = verify_all(&[Pin::new(&good, ABC), Pin::new(&bad, ABC)]).unwrap_err();

        assert!(matches!(&err, IntegrityError::Mismatch { path, .. } if path == &bad), "{err:?}");
    }

    #[test]
    fn tools_dir_prefers_the_override_and_otherwise_uses_the_profile() {
        let profile = Some(r"C:\Users\someone".into());

        assert_eq!(tools_dir_from(Some(r"D:\kit".into()), profile.clone()), PathBuf::from(r"D:\kit"));
        assert_eq!(tools_dir_from(None, profile.clone()), PathBuf::from(r"C:\Users\someone").join("tools"));
        assert_eq!(
            tools_dir_from(Some("".into()), profile),
            PathBuf::from(r"C:\Users\someone").join("tools"),
            "an empty override is no override"
        );
    }

    /// The point of returning a handle: between the check and the load, the
    /// file must not be swappable — but the loader must still be able to read
    /// it.
    #[cfg(windows)]
    #[test]
    fn a_verified_file_cannot_be_replaced_until_it_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_with(dir.path(), "whisper.dll", b"abc");
        let moved = dir.path().join("moved.dll");

        let verified = verify(&Pin::new(&path, ABC)).unwrap();

        assert!(std::fs::rename(&path, &moved).is_err(), "rename must be refused while held");
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err(), "writes must be refused while held");
        assert!(std::fs::remove_file(&path).is_err(), "deletion must be refused while held");
        assert_eq!(std::fs::read(&path).unwrap(), b"abc", "reading must still work while held");

        drop(verified);
        assert!(std::fs::rename(&path, &moved).is_ok(), "released, the file is ordinary again");
    }
}
