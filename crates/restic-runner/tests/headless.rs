use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn headless_profile_and_activity() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("fixture"), "headless fixture")?;
    let repository = temp.path().join("repository");
    let invoke = |arguments: &[&str]| -> anyhow::Result<std::process::Output> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_restic-studio"))
            .args(arguments)
            .env("XDG_CACHE_HOME", temp.path().join("cache"))
            .env("XDG_STATE_HOME", temp.path().join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(b"fixture secret\n")?;
        Ok(child.wait_with_output()?)
    };
    assert!(
        invoke(&["init", repository.to_str().unwrap()])?
            .status
            .success()
    );
    let directory = temp.path().join("state/restic-studio/profiles");
    let profile = restic_core::Profile {
        id: "test-profile".into(),
        name: "Headless test".into(),
        repository: restic_core::Repository {
            location: repository.to_string_lossy().into(),
        },
        sources: vec![source],
        excludes: vec![],
        one_file_system: false,
    };
    restic_core::save_profile(&profile, &directory)?;
    assert!(
        invoke(&[
            "preview",
            directory.join("test-profile.json").to_str().unwrap()
        ])?
        .status
        .success()
    );
    let before = invoke(&["snapshots", repository.to_str().unwrap()])?;
    assert_eq!(String::from_utf8(before.stdout)?.trim(), "[]");
    let result = invoke(&["run-profile", "test-profile"])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let history = fs::read_to_string(temp.path().join("state/restic-studio/activity.jsonl"))?;
    assert!(history.contains("Success"));
    assert!(!history.contains("fixture secret"));
    assert!(
        invoke(&["snapshots", repository.to_str().unwrap()])?
            .status
            .success()
    );
    Ok(())
}

#[test]
fn runner_manages_profiles_without_a_password() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    fs::create_dir(temp.path().join("source"))?;
    let invoke = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_restic-studio"))
            .args(arguments)
            .env("XDG_STATE_HOME", temp.path().join("state"))
            .stdin(Stdio::null())
            .output()
    };
    let profile = restic_core::Profile {
        id: "nightly".into(),
        name: "Nightly".into(),
        repository: restic_core::Repository {
            location: temp.path().join("repo").to_string_lossy().into(),
        },
        sources: vec![temp.path().join("source")],
        excludes: vec![],
        one_file_system: false,
    };
    let file = temp.path().join("nightly.json");
    fs::write(&file, serde_json::to_vec(&profile)?)?;
    assert!(
        invoke(&["profile-save", file.to_str().unwrap()])?
            .status
            .success()
    );
    let listed: serde_json::Value = serde_json::from_slice(&invoke(&["profiles"])?.stdout)?;
    assert_eq!(listed["profiles"][0]["id"], "nightly");
    assert!(invoke(&["profile-delete", "nightly"])?.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&invoke(&["profiles"])?.stdout)?;
    assert!(listed["profiles"].as_array().unwrap().is_empty());
    assert!(!invoke(&["profile-delete", "../nightly"])?.status.success());
    Ok(())
}
