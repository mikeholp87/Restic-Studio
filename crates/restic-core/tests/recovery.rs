use restic_core::*;
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
#[test]
fn root_source_can_use_a_repository_below_root() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let profile = Profile {
        id: "root".into(),
        name: "Root".into(),
        repository: Repository {
            location: temp.path().join("repo").to_string_lossy().into(),
        },
        sources: vec!["/".into()],
        excludes: vec![],
        one_file_system: false,
    };
    profile.validate()?;
    Ok(())
}
#[test]
fn disposable_recovery() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::create_dir(source.join("empty"))?;
    let filename = source.join("-日本語 [file] ?.txt");
    fs::write(&filename, b"original content")?;
    fs::write(source.join(".hidden"), b"hidden")?;
    fs::write(source.join("large"), vec![42u8; 2 * 1024 * 1024])?;
    std::os::unix::fs::symlink("-日本語 [file] ?.txt", source.join("link"))?;
    let outside = temp.path().join("outside");
    fs::write(&outside, b"must stay unchanged")?;
    std::os::unix::fs::symlink(&outside, source.join("external-link"))?;
    fs::set_permissions(&filename, fs::Permissions::from_mode(0o640))?;
    let repo = Repository {
        location: temp.path().join("repo").to_string_lossy().into(),
    };
    let engine = Engine {
        cache_dir: Some(temp.path().join("cache")),
        ..Engine::default()
    };
    let password = "fixture password";
    assert_eq!(engine.init(&repo, password)?.outcome, Outcome::Success);
    let profile = Profile {
        id: "fixture".into(),
        name: "Fixture".into(),
        repository: repo.clone(),
        sources: vec![source.clone()],
        excludes: vec![],
        one_file_system: false,
    };
    assert_eq!(
        engine
            .preview_backup(&profile, password, Arc::default(), |_| {})?
            .outcome,
        Outcome::Success
    );
    assert!(engine.snapshots(&repo, password)?.is_empty());
    assert_eq!(
        engine
            .backup(&profile, password, Arc::default(), |_| {})?
            .outcome,
        Outcome::Success
    );
    let first = engine.snapshots(&repo, password)?[0].id.clone();
    let mut listed = Vec::new();
    engine.list_directory(&repo, password, &first, source.to_str().unwrap(), |entry| {
        listed.push(entry)
    })?;
    assert!(
        listed
            .iter()
            .any(|entry| entry.path == filename.to_str().unwrap() && entry.kind == "file")
    );
    assert!(
        listed
            .iter()
            .any(|entry| entry.path.ends_with("/link") && entry.kind == "symlink")
    );
    assert!(
        engine
            .list_directory(&repo, password, &first, "/../etc", |_| {})
            .is_err()
    );
    fs::write(&filename, b"changed")?;
    fs::remove_file(source.join(".hidden"))?;
    assert_eq!(
        engine
            .backup(&profile, password, Arc::default(), |_| {})?
            .outcome,
        Outcome::Success
    );
    assert_eq!(engine.snapshots(&repo, password)?.len(), 2);
    let cancelled_destination = temp.path().join("cancelled-restore");
    assert_eq!(
        engine
            .restore_new_with_progress(
                &repo,
                password,
                &first,
                source.to_str().unwrap(),
                &cancelled_destination,
                Arc::new(AtomicBool::new(true)),
                |_| {},
            )?
            .outcome,
        Outcome::Cancelled
    );
    assert!(!cancelled_destination.exists());
    let destination = temp.path().join("restored");
    engine.restore_new(
        &repo,
        password,
        &first,
        source.to_str().unwrap(),
        &destination,
    )?;
    let restored = destination.join(source.strip_prefix("/")?);
    assert_eq!(
        fs::read(restored.join(filename.file_name().unwrap()))?,
        b"original content"
    );
    assert_eq!(fs::read(restored.join(".hidden"))?, b"hidden");
    assert_eq!(fs::read(restored.join("large"))?, vec![42; 2 * 1024 * 1024]);
    assert!(restored.join("empty").is_dir());
    assert_eq!(
        fs::read_link(restored.join("link"))?,
        std::path::PathBuf::from("-日本語 [file] ?.txt")
    );
    assert_eq!(fs::read_link(restored.join("external-link"))?, outside);
    assert_eq!(fs::read(&outside)?, b"must stay unchanged");
    assert_eq!(
        fs::metadata(restored.join(filename.file_name().unwrap()))?
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    assert!(
        engine
            .restore_new(&repo, password, &first, "/../etc", &temp.path().join("bad"))
            .is_err()
    );
    assert!(
        engine
            .restore_new(
                &repo,
                password,
                &first,
                source.to_str().unwrap(),
                &destination
            )
            .is_err()
    );
    let single = temp.path().join("single");
    engine.restore_new(&repo, password, &first, filename.to_str().unwrap(), &single)?;
    assert_eq!(
        fs::read(single.join(filename.strip_prefix("/")?))?,
        b"original content"
    );
    assert_eq!(
        engine
            .run(&repo, password, &["check".into()], Arc::default(), |_| {})?
            .outcome,
        Outcome::Success
    );
    assert!(engine.snapshots(&repo, "wrong").is_err());
    assert!(engine.init(&repo, password).is_err());
    let recursive = Profile {
        sources: vec![temp.path().to_path_buf()],
        ..profile.clone()
    };
    assert!(recursive.validate().is_err());
    let missing = Profile {
        repository: Repository {
            location: temp.path().join("absent").to_string_lossy().into(),
        },
        ..profile
    };
    assert!(
        engine
            .backup(&missing, password, Arc::default(), |_| {})
            .is_err()
    );
    Ok(())
}
#[test]
fn outcomes_and_identifiers() {
    assert_eq!(classify(Some(3), false, true), Outcome::Partial);
    assert_eq!(classify(Some(3), false, false), Outcome::Failed);
    assert_eq!(classify(Some(12), false, true), Outcome::WrongPassword);
    assert_eq!(classify(None, true, true), Outcome::Cancelled);
    assert!(valid_id("../../foo").is_err());
    assert!(snapshot_id("latest").is_err());
}
