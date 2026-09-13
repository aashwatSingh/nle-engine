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
    /// The file decoded, but describes a project the editor's own
    /// operations could never have produced.
    Corrupt(timeline::model::invariants::ProjectViolation),
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

/// Loads a project, migrating it forward if it was written by an older
/// build.
///
/// Spec 4.7's rule is "never ship a format change without a migration". The
/// previous implementation rejected anything whose version wasn't exactly
/// current, which meant the first schema bump would have made every existing
/// project file unopenable — the precise failure that rule exists to
/// prevent. Now unknown-but-supported versions run through `migrate`.
pub fn load(path: &Path) -> Result<ProjectDocument, LoadError> {
    let file = File::open(path)?;
    let doc: ProjectDocument = ciborium::from_reader(file).map_err(LoadError::Decode)?;
    let mut doc = migrate(doc)?;
    // Decoding proves the file is shaped like a project; this proves it
    // describes one. `edit_ops::apply` re-checks the whole project after
    // every single edit and refuses to hand back a result that breaks
    // these rules — so without the same check here, a file is the one way
    // into the editor that skips it entirely. What arrives that way isn't
    // merely wrong, it's unworkable: `apply` validates the *whole* project,
    // so one pre-existing overlap makes every future edit fail, and the
    // error names the edit the user just tried instead of the file.
    //
    // Refusing rather than repairing, for the reason the schema-version
    // check just above gives: guessing at what a broken file meant is how
    // you corrupt the user's work on the next save.
    timeline::model::invariants::check_project(&mut doc.project).map_err(LoadError::Corrupt)?;
    // Undo history is held to the same standard but is not worth refusing
    // the file over — it's recoverable context, not the work itself, and
    // schema v3 already set the precedent of dropping history it couldn't
    // trust. Left in place, a corrupt entry would simply move the problem
    // one Ctrl+Z away.
    if doc
        .undo_history
        .iter_mut()
        .any(|entry| timeline::model::invariants::check_project(&mut entry.before).is_err())
    {
        doc.undo_history.clear();
    }
    Ok(doc)
}

/// Brings `doc` up to `CURRENT_SCHEMA_VERSION`, one version at a time.
///
/// Written as a loop over single-step migrations rather than a match on
/// (from, to) pairs: a v1 document three versions behind then only needs
/// each step to be correct against its immediate successor, instead of
/// needing an N² table of direct conversions kept in sync.
pub fn migrate(mut doc: ProjectDocument) -> Result<ProjectDocument, LoadError> {
    if doc.schema_version > CURRENT_SCHEMA_VERSION
        || doc.schema_version < crate::schema::OLDEST_SUPPORTED_SCHEMA_VERSION
    {
        // Newer than this build understands, or older than we still support.
        // Refusing is right for both: guessing at a future layout would
        // corrupt the user's work on the next save.
        return Err(LoadError::UnsupportedSchemaVersion(doc.schema_version));
    }

    while doc.schema_version < CURRENT_SCHEMA_VERSION {
        doc = match doc.schema_version {
            // v1 -> v2: `Project::bins` was added. It's `#[serde(default)]`,
            // so a v1 document has already decoded with an empty bin list —
            // which is exactly right, since a v1 project had no folders and
            // everything belongs at the root. Nothing to rewrite; just record
            // that it's now a v2 document.
            1 => ProjectDocument { schema_version: 2, ..doc },
            // v2 -> v3: `Track::transitions` was added, also
            // `#[serde(default)]`. A v2 project had no transitions, so an
            // empty list per track is the correct reading; only the stamp
            // changes.
            2 => ProjectDocument { schema_version: 3, ..doc },
            // v3 -> v4: `PersistedCommand` dropped its `after` snapshot. A v3
            // document still decodes — serde ignores the now-unknown field —
            // and the chain it described is reconstructed on load, so the
            // dropped copy carried no information. Nothing to rewrite.
            3 => ProjectDocument { schema_version: 4, ..doc },
            // v4 -> v5: `ClipSource` gained a `Title` variant. A v4 document
            // cannot contain one, so every clip it does contain still decodes
            // exactly as before — adding an enum variant is backward
            // compatible in a way adding a *field* is not. The stamp still
            // moves so that a v5 project (which may contain titles) is
            // correctly refused by a v4 build rather than silently losing its
            // titles on the next save.
            4 => ProjectDocument { schema_version: 5, ..doc },
            // v5 -> v6: `Track::gain_db` became a `ParamTrack` (keyframeable
            // fader automation) instead of a bare `f64`. Nothing to rewrite
            // *here* — and deliberately so: a type change has to be handled
            // during decoding, since a v5 document's bare CBOR float fails to
            // deserialize into a `ParamTrack` before this function ever runs.
            // `timeline::model::deserialize_gain_db` does it, turning an old
            // fader into a constant track with the same value. This step
            // exists so the stamp still moves and a v6 project (which may hold
            // real automation) is refused by a v5 build rather than opened
            // with its automation silently flattened.
            5 => ProjectDocument { schema_version: 6, ..doc },
            // Unreachable while the bounds check above holds, but a real
            // branch rather than a panic: a future version added to the
            // constant without a matching arm should fail loudly at load
            // time, not corrupt data.
            other => return Err(LoadError::UnsupportedSchemaVersion(other)),
        };
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ProjectDocument;
    use timeline::{Project, TIMEBASE};

    #[test]
    fn round_trips_an_empty_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.nleproj");
        let doc = ProjectDocument::new(Project { sequences: vec![], assets: vec![], bins: vec![] }, vec![]);

        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.schema_version, doc.schema_version);
        assert_eq!(loaded.project, doc.project);
    }

    /// A one-track, one-clip sequence built field by field, so each test
    /// below can break exactly one rule and leave the rest valid.
    fn sequence_with(clips: Vec<timeline::ClipInstance>) -> timeline::Sequence {
        timeline::Sequence {
            id: timeline::SequenceId(1),
            name: "S1".into(),
            settings: timeline::SequenceSettings {
                frame_rate: timeline::FrameRate::Fps30,
                width: 1920,
                height: 1080,
                sample_rate: 48_000,
                working_color_primaries: media::ColorPrimaries::Rec709,
                drop_frame_timecode: false,
            },
            tracks: vec![timeline::Track {
                id: timeline::TrackId(1),
                kind: timeline::TrackKind::Video,
                name: "V1".into(),
                clips,
                transitions: vec![],
                gain_db: timeline::unity_gain(),
                pan: 0.0,
                locked: false,
                sync_locked: true,
                muted: false,
                solo: false,
                height_px: 60,
            }],
            markers: vec![],
        }
    }

    fn clip_at(id: u64, timeline_in: i64, timeline_out: i64) -> timeline::ClipInstance {
        timeline::ClipInstance {
            id: timeline::ClipInstanceId(id),
            source: timeline::ClipSource::Media(media::MediaAssetId(1)),
            source_in: timeline::TimeTick(0),
            source_out: timeline::TimeTick(timeline_out - timeline_in),
            timeline_in: timeline::TimeTick(timeline_in),
            timeline_out: timeline::TimeTick(timeline_out),
            speed: timeline::SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(timeline::ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(timeline::ParamValue::Number(0.0)),
            linked_group: None,
        }
    }

    fn save_and_load(sequence: timeline::Sequence) -> Result<ProjectDocument, LoadError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.nleproj");
        let doc = ProjectDocument::new(
            Project { sequences: vec![sequence], assets: vec![], bins: vec![] },
            vec![],
        );
        save(&doc, &path).unwrap();
        load(&path)
    }

    #[test]
    fn a_project_whose_clips_overlap_is_refused_rather_than_opened_unusable() {
        // The failure this exists to prevent isn't the overlap itself, it's
        // what the overlap does to everything after: `edit_ops::apply`
        // validates the whole project after every edit, so a file carrying
        // one makes every subsequent edit fail — and blames the edit.
        let loaded = save_and_load(sequence_with(vec![
            clip_at(1, 0, 2 * TIMEBASE),
            clip_at(2, TIMEBASE, 3 * TIMEBASE),
        ]));
        assert!(
            matches!(loaded, Err(LoadError::Corrupt(_))),
            "an overlapping project must be refused, got {loaded:?}"
        );
    }

    #[test]
    fn a_clip_that_ends_before_it_starts_is_refused() {
        let loaded = save_and_load(sequence_with(vec![clip_at(1, 2 * TIMEBASE, TIMEBASE)]));
        assert!(matches!(loaded, Err(LoadError::Corrupt(_))), "got {loaded:?}");
    }

    #[test]
    fn two_clips_sharing_one_id_are_refused() {
        // Every lookup in the editor is by id, so a duplicate doesn't fail
        // loudly — it silently edits whichever one was found first.
        let loaded = save_and_load(sequence_with(vec![
            clip_at(7, 0, TIMEBASE),
            clip_at(7, TIMEBASE, 2 * TIMEBASE),
        ]));
        assert!(matches!(loaded, Err(LoadError::Corrupt(_))), "got {loaded:?}");
    }

    #[test]
    fn an_unusable_sample_rate_is_refused() {
        let mut sequence = sequence_with(vec![clip_at(1, 0, TIMEBASE)]);
        sequence.settings.sample_rate = 0;
        let loaded = save_and_load(sequence);
        assert!(matches!(loaded, Err(LoadError::Corrupt(_))), "got {loaded:?}");
    }

    #[test]
    fn a_clip_starting_before_zero_is_refused() {
        let loaded = save_and_load(sequence_with(vec![clip_at(1, -TIMEBASE, TIMEBASE)]));
        assert!(matches!(loaded, Err(LoadError::Corrupt(_))), "got {loaded:?}");
    }

    #[test]
    fn clips_stored_out_of_order_are_sorted_rather_than_rejected() {
        // Storage order is not corruption: `apply` sorts before it checks,
        // because operations that move a clip in place leave the vec
        // unsorted without anything being wrong. Rejecting it would refuse
        // files the editor itself writes.
        let loaded = save_and_load(sequence_with(vec![
            clip_at(2, 2 * TIMEBASE, 3 * TIMEBASE),
            clip_at(1, 0, TIMEBASE),
        ]))
        .expect("out-of-order storage must still open");
        let clips = &loaded.project.sequences[0].tracks[0].clips;
        assert_eq!(
            clips.iter().map(|c| c.id.0).collect::<Vec<_>>(),
            vec![1, 2],
            "and must come back in timeline order"
        );
    }

    #[test]
    fn an_ordinary_project_still_opens() {
        // The guard against a validator so strict it refuses real work.
        let loaded = save_and_load(sequence_with(vec![
            clip_at(1, 0, TIMEBASE),
            clip_at(2, TIMEBASE, 2 * TIMEBASE),
        ]));
        assert!(loaded.is_ok(), "a valid project must still load: {loaded:?}");
    }

    #[test]
    fn corrupt_undo_history_is_dropped_without_refusing_the_file() {
        // History is recoverable context, not the work itself. Left in
        // place a corrupt entry would just move the problem one Ctrl+Z away.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.nleproj");
        let good = Project {
            sequences: vec![sequence_with(vec![clip_at(1, 0, TIMEBASE)])],
            assets: vec![],
            bins: vec![],
        };
        let corrupt = Project {
            sequences: vec![sequence_with(vec![
                clip_at(1, 0, 2 * TIMEBASE),
                clip_at(2, TIMEBASE, 3 * TIMEBASE),
            ])],
            assets: vec![],
            bins: vec![],
        };
        let mut doc = ProjectDocument::new(good, vec![]);
        doc.undo_history = vec![crate::schema::PersistedCommand {
            label: "poisoned".into(),
            before: corrupt,
        }];
        save(&doc, &path).unwrap();

        let loaded = load(&path).expect("a sound project must still open");
        assert!(loaded.undo_history.is_empty(), "history that fails the same check must be dropped");
    }

    #[test]
    fn a_v5_project_whose_fader_was_a_bare_number_still_opens_at_the_same_level() {
        // The migration that can't live in `migrate`. v5 wrote `gain_db` as a
        // CBOR float; v6 expects a `ParamTrack`. Decoding happens *before*
        // migration, so without the compat deserializer this file wouldn't
        // fail to migrate — it would fail to open at all, taking the user's
        // whole project with it. Built as raw CBOR rather than by
        // round-tripping this build's own types, because a test that writes
        // with today's serialiser can never prove yesterday's files load.
        use ciborium::value::Value;

        let track = Value::Map(vec![
            (Value::Text("id".into()), Value::Map(vec![])),
            (Value::Text("kind".into()), Value::Text("Audio".into())),
            (Value::Text("name".into()), Value::Text("A1".into())),
            (Value::Text("clips".into()), Value::Array(vec![])),
            // The v5 shape: a bare number, not a keyframe track.
            (Value::Text("gain_db".into()), Value::Float(-6.0)),
            (Value::Text("pan".into()), Value::Float(0.0)),
            (Value::Text("locked".into()), Value::Bool(false)),
            (Value::Text("sync_locked".into()), Value::Bool(true)),
            (Value::Text("muted".into()), Value::Bool(false)),
            (Value::Text("solo".into()), Value::Bool(false)),
            (Value::Text("height_px".into()), Value::Integer(60.into())),
        ]);
        // `TrackId` is a newtype over u64, which serialises as a bare integer.
        let track = match track {
            Value::Map(mut entries) => {
                entries[0].1 = Value::Integer(1.into());
                Value::Map(entries)
            }
            other => other,
        };

        let decoded: Result<timeline::Track, _> = track.deserialized();
        let decoded = decoded.expect("a v5 track with a bare-number fader must still decode");
        assert_eq!(
            decoded.gain_db.evaluate_at(timeline::TimeTick(0)).as_scalar(),
            Some(-6.0),
            "the old fader value must survive as a constant automation track"
        );
        assert!(!decoded.gain_db.is_animated(), "and must not look automated");
    }

    #[test]
    fn a_title_clip_survives_a_save_and_reload_with_its_text_intact() {
        // A title carries its whole appearance inline rather than referencing
        // an asset, so unlike every other clip source there is no separate
        // table that would catch a serialisation mistake. If the spec doesn't
        // round-trip, the user's words are simply gone.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("titled.nleproj");
        let spec = timeline::TitleSpec {
            text: "Two\nLines".into(),
            font_family: "Consolas".into(),
            size_px: 61.5,
            color: [0.25, 0.5, 0.75, 0.9],
            align: timeline::TextAlign::Right,
            position: (0.125, 0.875),
        };
        let clip = timeline::ClipInstance {
            id: timeline::ClipInstanceId(1),
            source: timeline::ClipSource::Title(spec.clone()),
            source_in: timeline::TimeTick(0),
            source_out: timeline::TimeTick(100),
            timeline_in: timeline::TimeTick(0),
            timeline_out: timeline::TimeTick(100),
            speed: timeline::SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(timeline::ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(timeline::ParamValue::Number(0.0)),
            linked_group: None,
        };
        let project = Project {
            sequences: vec![timeline::Sequence {
                id: timeline::SequenceId(1),
                name: "S".into(),
                settings: timeline::SequenceSettings {
                    frame_rate: timeline::FrameRate::Fps30,
                    width: 64,
                    height: 64,
                    sample_rate: 48_000,
                    working_color_primaries: media::ColorPrimaries::Rec709,
                    drop_frame_timecode: false,
                },
                tracks: vec![timeline::Track {
                    id: timeline::TrackId(1),
                    kind: timeline::TrackKind::Video,
                    name: "V1".into(),
                    clips: vec![clip],
                    transitions: vec![],
                    gain_db: timeline::unity_gain(),
                    pan: 0.0,
                    locked: false,
                    sync_locked: true,
                    muted: false,
                    solo: false,
                    height_px: 60,
                }],
                markers: vec![],
            }],
            assets: vec![],
            bins: vec![],
        };

        save(&ProjectDocument::new(project, vec![]), &path).unwrap();
        let loaded = load(&path).unwrap();

        let source = &loaded.project.sequences[0].tracks[0].clips[0].source;
        let timeline::ClipSource::Title(round_tripped) = source else {
            panic!("the clip came back as {source:?}, not a title");
        };
        // Field by field rather than one equality, so a failure names which
        // part of the title was lost.
        assert_eq!(round_tripped.text, spec.text, "the text itself");
        assert_eq!(round_tripped.font_family, spec.font_family);
        assert_eq!(round_tripped.size_px, spec.size_px);
        assert_eq!(round_tripped.color, spec.color);
        assert_eq!(round_tripped.align, spec.align);
        assert_eq!(round_tripped.position, spec.position);
    }

    #[test]
    fn no_temp_file_left_behind_after_successful_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.nleproj");
        let doc = ProjectDocument::new(Project { sequences: vec![], assets: vec![], bins: vec![] }, vec![]);

        save(&doc, &path).unwrap();

        assert!(path.exists());
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn rejects_a_version_newer_than_this_build() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.nleproj");
        let mut doc = ProjectDocument::new(Project { sequences: vec![], assets: vec![], bins: vec![] }, vec![]);
        doc.schema_version = 999;

        save(&doc, &path).unwrap();
        let result = load(&path);

        assert!(matches!(result, Err(LoadError::UnsupportedSchemaVersion(999))));
    }

    /// A v1 document, as written before `bins` existed: no `bins` key at all.
    /// Hand-built as a CBOR map rather than by serialising a current struct,
    /// because a current struct would always include the new field and the
    /// test would prove nothing about real old files.
    fn write_v1_document_without_bins(path: &Path) {
        use ciborium::value::Value;
        let project = Value::Map(vec![
            (Value::Text("sequences".into()), Value::Array(vec![])),
            (Value::Text("assets".into()), Value::Array(vec![])),
            // deliberately no "bins"
        ]);
        let doc = Value::Map(vec![
            (Value::Text("schema_version".into()), Value::Integer(1.into())),
            (Value::Text("project".into()), project),
            (Value::Text("media_references".into()), Value::Array(vec![])),
            (Value::Text("undo_history".into()), Value::Array(vec![])),
        ]);
        let file = File::create(path).unwrap();
        ciborium::into_writer(&doc, file).unwrap();
    }

    #[test]
    fn a_v1_project_written_before_bins_existed_still_opens() {
        // The regression this whole migration chain exists for: `load` used to
        // reject any version != current, so bumping the schema would have made
        // every previously-saved project unopenable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.nleproj");
        write_v1_document_without_bins(&path);

        let loaded = load(&path).expect("a v1 project must still open");

        assert_eq!(loaded.schema_version, CURRENT_SCHEMA_VERSION, "should be migrated forward");
        assert!(loaded.project.bins.is_empty(), "a v1 project had no folders, so everything is at the root");
    }

    /// A v2 document: has `bins`, but its tracks predate `transitions`.
    /// Hand-built CBOR rather than an old struct definition, because the point
    /// is to exercise the *bytes* a previous build actually wrote.
    fn write_v2_document_without_transitions(path: &Path) {
        use ciborium::value::Value;
        let track = Value::Map(vec![
            (Value::Text("id".into()), Value::Integer(1.into())),
            (Value::Text("kind".into()), Value::Text("Video".into())),
            (Value::Text("name".into()), Value::Text("V1".into())),
            (Value::Text("clips".into()), Value::Array(vec![])),
            // deliberately no "transitions"
            (Value::Text("locked".into()), Value::Bool(false)),
            (Value::Text("sync_locked".into()), Value::Bool(true)),
            (Value::Text("muted".into()), Value::Bool(false)),
            (Value::Text("solo".into()), Value::Bool(false)),
            (Value::Text("height_px".into()), Value::Integer(60.into())),
        ]);
        let settings = Value::Map(vec![
            (Value::Text("frame_rate".into()), Value::Text("Fps30".into())),
            (Value::Text("width".into()), Value::Integer(1920.into())),
            (Value::Text("height".into()), Value::Integer(1080.into())),
            (Value::Text("sample_rate".into()), Value::Integer(48_000.into())),
            (Value::Text("working_color_primaries".into()), Value::Text("Rec709".into())),
            (Value::Text("drop_frame_timecode".into()), Value::Bool(false)),
        ]);
        let sequence = Value::Map(vec![
            (Value::Text("id".into()), Value::Integer(1.into())),
            (Value::Text("name".into()), Value::Text("Sequence 01".into())),
            (Value::Text("settings".into()), settings),
            (Value::Text("tracks".into()), Value::Array(vec![track])),
            (Value::Text("markers".into()), Value::Array(vec![])),
        ]);
        let project = Value::Map(vec![
            (Value::Text("sequences".into()), Value::Array(vec![sequence])),
            (Value::Text("assets".into()), Value::Array(vec![])),
            (Value::Text("bins".into()), Value::Array(vec![])),
        ]);
        let doc = Value::Map(vec![
            (Value::Text("schema_version".into()), Value::Integer(2.into())),
            (Value::Text("project".into()), project),
            (Value::Text("media_references".into()), Value::Array(vec![])),
            (Value::Text("undo_history".into()), Value::Array(vec![])),
        ]);
        let file = File::create(path).unwrap();
        ciborium::into_writer(&doc, file).unwrap();
    }

    #[test]
    fn a_v2_project_written_before_transitions_existed_still_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.nleproj");
        write_v2_document_without_transitions(&path);

        let loaded = load(&path).expect("a v2 project must still open");

        assert_eq!(loaded.schema_version, CURRENT_SCHEMA_VERSION);
        let track = &loaded.project.sequences[0].tracks[0];
        assert!(
            track.transitions.is_empty(),
            "a v2 project had no transitions, so an empty list is the correct reading"
        );
        // And the rest of the track survived the migration intact, rather than
        // being defaulted away along with the new field.
        assert_eq!(track.name, "V1");
        assert_eq!(track.height_px, 60);
    }

    #[test]
    fn every_supported_version_migrates_to_current() {
        // Guards the chain itself: each recorded version must have a path
        // forward. A version constant bumped without a matching `migrate` arm
        // would otherwise only fail on a user's real file.
        for v in crate::schema::OLDEST_SUPPORTED_SCHEMA_VERSION..=CURRENT_SCHEMA_VERSION {
            let doc = ProjectDocument {
                schema_version: v,
                project: Project { sequences: vec![], assets: vec![], bins: vec![] },
                media_references: vec![],
                undo_history: vec![],
            };
            let migrated = migrate(doc).unwrap_or_else(|e| panic!("v{v} has no migration path: {e:?}"));
            assert_eq!(migrated.schema_version, CURRENT_SCHEMA_VERSION);
        }
    }

    #[test]
    fn migrating_then_saving_writes_the_current_version() {
        // A migrated document must persist as current, or it would be
        // re-migrated on every open forever.
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.nleproj");
        let new = dir.path().join("new.nleproj");
        write_v1_document_without_bins(&old);

        let migrated = load(&old).unwrap();
        save(&migrated, &new).unwrap();

        assert_eq!(load(&new).unwrap().schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn bins_survive_a_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bins.nleproj");
        let project = Project {
            sequences: vec![],
            assets: vec![],
            bins: vec![
                timeline::Bin {
                    id: timeline::BinId(1),
                    name: "Footage".into(),
                    parent: None,
                    items: vec![timeline::BinItem::Asset(media::MediaAssetId(7))],
                },
                timeline::Bin {
                    id: timeline::BinId(2),
                    name: "B-roll".into(),
                    parent: Some(timeline::BinId(1)),
                    items: vec![],
                },
            ],
        };
        let doc = ProjectDocument::new(project.clone(), vec![]);

        save(&doc, &path).unwrap();
        let loaded = load(&path).unwrap();

        assert_eq!(loaded.project.bins, project.bins, "bin tree must persist exactly");
    }
}
