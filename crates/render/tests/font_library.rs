//! `FontLibrary` resolves a requested family against the font's own **name
//! table**, not just its filename — a title made on another machine names
//! the family the user picked from a font-chooser UI ("Snap ITC"), never the
//! file it happens to live in (`SNAP____.TTF`), and for most installed fonts
//! those two strings simply don't match.

use render::text::FontLibrary;

#[test]
fn a_family_whose_filename_bears_no_resemblance_to_it_still_resolves() {
    // Windows ships SNAP____.TTF whose declared family name is "Snap ITC" —
    // no alias table entry could plausibly cover this by guesswork, so
    // finding it proves the resolution is reading the font's own name table,
    // not pattern-matching filenames.
    let path = r"C:\Windows\Fonts\SNAP____.TTF";
    if !std::path::Path::new(path).exists() {
        eprintln!("skipping: Snap ITC isn't installed on this machine");
        return;
    }
    let lib = FontLibrary::system();
    let found = lib.face("Snap ITC").expect("Snap ITC should resolve by its real family name");
    let expected = std::fs::read(path).unwrap();
    assert_eq!(
        found.raw_bytes(),
        expected.as_slice(),
        "resolved to the wrong file for the requested family"
    );
}

#[test]
fn family_name_matching_is_case_and_space_insensitive() {
    let path = r"C:\Windows\Fonts\SNAP____.TTF";
    if !std::path::Path::new(path).exists() {
        eprintln!("skipping: Snap ITC isn't installed on this machine");
        return;
    }
    let lib = FontLibrary::system();
    let canonical = lib.face("Snap ITC").expect("precondition: exact case resolves");
    let lower = lib.face("snap itc").expect("lowercase should still match");
    let upper = lib.face("SNAP ITC").expect("uppercase should still match");
    assert_eq!(lower.raw_bytes(), canonical.raw_bytes());
    assert_eq!(upper.raw_bytes(), canonical.raw_bytes());
}

#[test]
fn available_families_reports_real_names_not_file_stems() {
    let lib = FontLibrary::system();
    let families = lib.available_families();
    assert!(
        families.iter().any(|f| f == "Arial"),
        "expected a real family name like \"Arial\" in the list, got e.g. {:?}",
        families.iter().take(5).collect::<Vec<_>>()
    );
    assert!(
        !families.iter().any(|f| f.eq_ignore_ascii_case("snap____")),
        "the list should never contain a raw file stem"
    );
}

#[test]
fn an_unresolvable_family_still_falls_back_rather_than_returning_none() {
    let lib = FontLibrary::system();
    assert!(
        lib.face("No Such Font Anyone Has Ever Installed").is_some(),
        "an unknown family should fall back to a default, not fail outright"
    );
}
