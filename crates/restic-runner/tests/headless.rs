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
