use restic_core::*;
use std::{fs, os::unix::fs::PermissionsExt};

fn profile(temp: &std::path::Path, id: &str, name: &str) -> Profile {
    Profile {
        id: id.into(),
        name: name.into(),
        repository: Repository {
            location: temp.join("repo").to_string_lossy().into(),
        },
        sources: vec![temp.join("source")],
        excludes: vec!["*.tmp".into()],
        one_file_system: true,
    }
}

#[test]
fn profiles_save_list_load_delete() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    fs::create_dir(temp.path().join("source"))?;
    let store = temp.path().join("profiles");
    assert!(
        list_profiles(&store)?.profiles.is_empty(),
        "missing store lists empty"
    );

    save_profile(&profile(temp.path(), "work", "Work files"), &store)?;
    save_profile(&profile(temp.path(), "docs", "alpha documents"), &store)?;
    assert_eq!(
        fs::metadata(store.join("work.json"))?.permissions().mode() & 0o777,
        0o600
    );
    let listed = list_profiles(&store)?;
    assert!(listed.problems.is_empty());
    let names: Vec<_> = listed.profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["alpha documents", "Work files"],
        "sorted, case-insensitive"
    );
    assert_eq!(load_profile(&store, "work")?.excludes, ["*.tmp"]);

    // Saving the same ID again replaces it.
    save_profile(&profile(temp.path(), "work", "Work renamed"), &store)?;
    assert_eq!(load_profile(&store, "work")?.name, "Work renamed");

    delete_profile(&store, "work")?;
    assert!(load_profile(&store, "work").is_err());
    assert!(delete_profile(&store, "work").is_err(), "already gone");
    assert_eq!(list_profiles(&store)?.profiles.len(), 1);
    Ok(())
}

#[test]
fn bad_profile_files_are_reported_without_hiding_good_ones() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    fs::create_dir(temp.path().join("source"))?;
    let store = temp.path().join("profiles");
    save_profile(&profile(temp.path(), "good", "Good"), &store)?;
    fs::write(store.join("broken.json"), "{ not json")?;
    // A file whose content claims another identity must not be trusted under this name.
    fs::write(
        store.join("liar.json"),
        serde_json::to_vec(&profile(temp.path(), "good", "Impostor"))?,
    )?;
    fs::write(store.join("notes.txt"), "ignored")?;
    let listed = list_profiles(&store)?;
    assert_eq!(listed.profiles.len(), 1);
    assert_eq!(listed.profiles[0].name, "Good");
    assert_eq!(listed.problems.len(), 2, "{:?}", listed.problems);
    assert!(load_profile(&store, "liar").is_err());
    Ok(())
}

#[test]
fn profile_ids_cannot_escape_the_store() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let store = temp.path().join("profiles");
    fs::create_dir(&store)?;
    fs::write(temp.path().join("outside.json"), "{}")?;
    assert!(load_profile(&store, "../outside").is_err());
    assert!(delete_profile(&store, "../outside").is_err());
    assert!(temp.path().join("outside.json").exists());
    Ok(())
}

#[test]
fn an_absent_drive_does_not_hide_a_profile() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    fs::create_dir(temp.path().join("source"))?;
    let store = temp.path().join("profiles");
    save_profile(&profile(temp.path(), "usb", "USB backup"), &store)?;
    fs::remove_dir(temp.path().join("source"))?;
    // validate() fails now (source gone), but the profile must still list and load.
    assert_eq!(list_profiles(&store)?.profiles.len(), 1);
    assert!(load_profile(&store, "usb")?.validate().is_err());
    Ok(())
}
