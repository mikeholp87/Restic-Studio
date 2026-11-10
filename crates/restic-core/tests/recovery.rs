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
        state_dir: Some(temp.path().join("state")),
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
            .check(&repo, password, Arc::default(), |_| {})?
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
fn swapped_repository_is_refused() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("file"), b"data")?;
    let repo = Repository {
        location: temp.path().join("repo").to_string_lossy().into(),
    };
    let engine = Engine {
        cache_dir: Some(temp.path().join("cache")),
        state_dir: Some(temp.path().join("state")),
        ..Engine::default()
    };
    let profile = Profile {
        id: "swap".into(),
        name: "Swap".into(),
        repository: repo.clone(),
        sources: vec![source],
        excludes: vec![],
        one_file_system: false,
    };
    engine.init(&repo, "pw one")?;
    let first = engine.repository_id(&repo, "pw one")?;
    assert_eq!(first.len(), 64);
    // First backup pins the ID; a second backup to the same repository is accepted.
    engine.backup(&profile, "pw one", Arc::default(), |_| {})?;
    engine.backup(&profile, "pw one", Arc::default(), |_| {})?;
    // A different repository appears at the same path.
    fs::remove_dir_all(temp.path().join("repo"))?;
    engine.init(&repo, "pw two")?;
    assert_ne!(engine.repository_id(&repo, "pw two")?, first);
    let error = engine
        .backup(&profile, "pw two", Arc::default(), |_| {})
        .unwrap_err();
    assert!(error.to_string().contains("identity changed"), "{error}");
    // Browsing and checking the impostor are refused too, not just writing to it.
    for refused in [
        engine.snapshots(&repo, "pw two").map(|_| ()),
        engine
            .check(&repo, "pw two", Arc::default(), |_| {})
            .map(|_| ()),
    ] {
        let error = refused.unwrap_err();
        assert!(error.to_string().contains("identity changed"), "{error}");
    }
    Ok(())
}
/// A stand-in Restic that prints `count` snapshots as one JSON line.
fn fake_restic(dir: &std::path::Path, count: u32) -> anyhow::Result<Engine> {
    // Install via an external `install` so this process never holds the script open for
    // writing; a parallel test's fork would otherwise make exec fail with ETXTBSY.
    let source = dir.join("restic.src");
    let script = dir.join("restic");
    fs::write(
        &source,
        format!(
            "#!/bin/sh\ncat >/dev/null\nawk 'BEGIN{{printf \"[\"; for(i=0;i<{count};i++){{if(i)printf \",\"; printf \"{{\\\"id\\\":\\\"%064d\\\",\\\"time\\\":\\\"2026-01-01T00:00:00Z\\\",\\\"paths\\\":[\\\"/home\\\"],\\\"hostname\\\":\\\"h\\\",\\\"tags\\\":[]}}\", i}}; print \"]\"}}'\n"
        ),
    )?;
    anyhow::ensure!(
        std::process::Command::new("install")
            .arg("-m755")
            .arg(&source)
            .arg(&script)
            .status()?
            .success()
    );
    Ok(Engine {
        executable: script,
        state_dir: Some(dir.join("state")),
        ..Engine::default()
    })
}
#[test]
fn large_snapshot_list_is_parsed_within_the_limit() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = Repository {
        location: temp.path().to_string_lossy().into(),
    };
    // About 2.5 MiB on one line: over the generic 1 MiB record cap, under the snapshot cap.
    let engine = fake_restic(temp.path(), 20_000)?;
    assert_eq!(engine.snapshots(&repo, "pw")?.len(), 20_000);
    // About 44 MiB: over the snapshot cap, so listing fails explicitly instead of truncating.
    let engine = fake_restic(temp.path(), 400_000)?;
    let error = engine.snapshots(&repo, "pw").unwrap_err();
    assert!(error.to_string().contains("32 MiB"), "{error}");
    Ok(())
}
#[test]
fn concurrent_jobs_on_one_repository_are_refused() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("file"), b"data")?;
    let repo = |name: &str| Repository {
        location: temp.path().join(name).to_string_lossy().into(),
    };
    let (busy, idle) = (repo("busy"), repo("idle"));
    let engine = Engine {
        cache_dir: Some(temp.path().join("cache")),
        state_dir: Some(temp.path().join("state")),
        ..Engine::default()
    };
    engine.init(&busy, "pw")?;
    engine.init(&idle, "pw")?;
    let profile = Profile {
        id: "busy".into(),
        name: "Busy".into(),
        repository: busy.clone(),
        sources: vec![source],
        excludes: vec![],
        one_file_system: false,
    };
    let held = engine.lock_repository(&busy)?;
    // Another client claiming the same repository is refused, not queued.
    let error = engine
        .backup(&profile, "pw", Arc::default(), |_| {})
        .unwrap_err();
    assert!(
        error.to_string().contains("Another Restic Studio job"),
        "{error}"
    );
    assert!(engine.lock_repository(&busy).is_err());
    // A different repository is not affected.
    let _other = engine.lock_repository(&idle)?;
    // Releasing the claim lets the job run.
    drop(held);
    assert_eq!(
        engine
            .backup(&profile, "pw", Arc::default(), |_| {})?
            .outcome,
        Outcome::Success
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
