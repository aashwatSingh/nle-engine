//! Save/load with the atomic-write discipline spec 4.7 requires for
//! autosave: write to a temp file, fsync, rename over the target. A crash
//! mid-write must never leave a truncated project file in place of a good
//! one.

use crate::schema::{ProjectDocument, CURRENT_SCHEMA_VERSION};
use std::fs::File;
use std::io;
use std::path::Path;

#[derive(Debug)]
pub enum LoadError {
    Io(io::Error),
    Decode(ciborium::de::Error<io::Error>),
    UnsupportedSchemaVersion(u32),
}

impl From<io::Error> for LoadError {
    fn from(e: io::Error) -> Self {
        LoadError::Io(e)
    }
}

#[derive(Debug)]
pub enum SaveError {
    Io(io::Error),
    Encode(ciborium::ser::Error<io::Error>),
}

impl From<io::Error> for SaveError {
    fn from(e: io::Error) -> Self {
        SaveError::Io(e)
    }
}

/// Write-temp -> fsync -> rename. `path`'s parent directory must already
/// exist; the temp file is created alongside it so the rename is same-volume
/// (required for atomicity on both Windows and POSIX).
pub fn save(doc: &ProjectDocument, path: &Path) -> Result<(), SaveError> {
    let tmp_path = path.with_extension("tmp");
    {
        let file = File::create(&tmp_path)?;
        ciborium::into_writer(doc, &file).map_err(SaveError::Encode)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// TODO(post-M0): once `CURRENT_SCHEMA_VERSION` moves past 1, this must run
/// the document through a migration chain (spec 4.7 "never ship a format
/// change without a migration") before returning. There is no chain yet
/// because there is only one version — this rejects anything else rather
/// than silently guessing.
pub fn load(path: &Path) -> Result<ProjectDocument, LoadError> {
    let file = File::open(path)?;
    let doc: ProjectDocument = ciborium::from_reader(file).map_err(LoadError::Decode)?;
    if doc.schema_version != CURRENT_SCHEMA_VERSION {
        return Err(LoadError::UnsupportedSchemaVersion(doc.schema_version));
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ProjectDocument;
    use timeline::Project;

    #[test]
    fn round_trips_an_empty_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.nleproj");
        let doc = ProjectDocument::new(Project { sequences: vec![], assets: vec![] }, vec![]);

        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.schema_version, doc.schema_version);
        assert_eq!(loaded.project, doc.project);
    }

    #[test]
    fn no_temp_file_left_behind_after_successful_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.nleproj");
        let doc = ProjectDocument::new(Project { sequences: vec![], assets: vec![] }, vec![]);

        save(&doc, &path).unwrap();

        assert!(path.exists());
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.nleproj");
        let mut doc = ProjectDocument::new(Project { sequences: vec![], assets: vec![] }, vec![]);
        doc.schema_version = 999;

        save(&doc, &path).unwrap();
        let result = load(&path);

        assert!(matches!(result, Err(LoadError::UnsupportedSchemaVersion(999))));
    }
}
