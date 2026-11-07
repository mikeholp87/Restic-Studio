//! How `Engine::run` behaves when the Restic subprocess misbehaves.
//! A shell script stands in for Restic, so no real repository is needed.
use restic_core::*;
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

const PASSWORD: &str = "hunter2";

/// Publish `body` as an executable without this process ever holding it open for writing:
/// a parallel test's fork would otherwise inherit that handle and exec fails with ETXTBSY.
fn install_script(dir: &Path, body: &str) -> anyhow::Result<std::path::PathBuf> {
    let source = dir.join("restic.src");
    let script = dir.join("restic");
    fs::write(&source, body)?;
    anyhow::ensure!(
        std::process::Command::new("install")
            .arg("-m755")
            .arg(&source)
            .arg(&script)
            .status()?
            .success()
    );
    Ok(script)
}
/// An engine whose "restic" runs `body` after draining the password from stdin.
fn fake(dir: &Path, body: &str) -> anyhow::Result<(Engine, Repository)> {
    let script = install_script(dir, &format!("#!/bin/sh\ncat >/dev/null\n{body}\n"))?;
    let repo = Repository {
        location: dir.to_string_lossy().into(),
    };
    Ok((
        Engine {
            executable: script,
            ..Engine::default()
        },
        repo,
    ))
}
fn run(engine: &Engine, repo: &Repository, first_arg: &str) -> anyhow::Result<JobResult> {
    engine.run(repo, PASSWORD, &[first_arg.into()], Arc::default(), |_| {})
}

#[test]
fn exit_codes_map_to_outcomes() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    for (code, command, expected) in [
        (0, "snapshots", Outcome::Success),
        (3, "backup", Outcome::Partial),
        (3, "snapshots", Outcome::Failed),
        (10, "snapshots", Outcome::MissingRepository),
        (11, "snapshots", Outcome::Locked),
        (12, "snapshots", Outcome::WrongPassword),
        (1, "snapshots", Outcome::Failed),
        (255, "backup", Outcome::Failed),
    ] {
        let (engine, repo) = fake(temp.path(), &format!("exit {code}"))?;
        let result = run(&engine, &repo, command)?;
        assert_eq!(result.outcome, expected, "exit {code} for {command}");
        assert_eq!(result.exit_code, Some(code));
    }
    Ok(())
}

#[test]
fn a_crashing_subprocess_is_a_failure_not_a_hang() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (engine, repo) = fake(temp.path(), "kill -9 $$")?;
    let result = run(&engine, &repo, "backup")?;
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(result.exit_code, None, "a signal death has no exit code");
    Ok(())
}

#[test]
fn a_missing_executable_is_an_error() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let engine = Engine {
        executable: temp.path().join("absent"),
        ..Engine::default()
    };
    let repo = Repository {
        location: temp.path().to_string_lossy().into(),
    };
    let error = run(&engine, &repo, "snapshots").unwrap_err();
    assert!(
        error.to_string().contains("Could not start Restic"),
        "{error}"
    );
    Ok(())
}

#[test]
fn password_and_repository_path_are_redacted_from_output() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let location = temp.path().to_string_lossy().into_owned();
    let body = format!(
        "echo 'cannot open {location}/config with {PASSWORD}' >&2\n\
         echo '{{\"message_type\":\"exit_error\",\"message\":\"bad {PASSWORD} at {location}\"}}'\n\
         exit 12"
    );
    let (engine, repo) = fake(temp.path(), &body)?;
    let mut shown = String::new();
    let result = engine.run(
        &repo,
        PASSWORD,
        &["snapshots".into()],
        Arc::default(),
        |event| match event {
            Event::Diagnostic(text) => shown.push_str(&text),
            Event::Json(value) => shown.push_str(&value.to_string()),
        },
    )?;
    assert_eq!(result.outcome, Outcome::WrongPassword);
    assert_eq!(result.errors.len(), 2, "{:?}", result.errors);
    // Diagnostics (stderr) are redacted before reaching the caller.
    assert!(!result.errors.concat().contains(PASSWORD));
    assert!(!result.errors.concat().contains(&location));
    assert!(!shown.contains("with hunter2"), "{shown}");
    Ok(())
}

#[test]
fn error_reports_are_capped() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let body = "i=0; while [ $i -lt 50 ]; do echo \"problem $i\" >&2; i=$((i+1)); done\nexit 1";
    let (engine, repo) = fake(temp.path(), body)?;
    let result = run(&engine, &repo, "backup")?;
    assert_eq!(result.errors.len(), 8);
    Ok(())
}

#[test]
fn oversized_and_garbage_output_is_survived() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    // 2 MiB on one line, then non-JSON noise, then a normal exit.
    let body = "head -c 2097152 /dev/zero | tr '\\0' x\necho\necho 'not json at all'\nexit 0";
    let (engine, repo) = fake(temp.path(), body)?;
    let result = run(&engine, &repo, "snapshots")?;
    assert_eq!(result.outcome, Outcome::Success);
    assert!(
        result.errors.iter().any(|e| e.contains("exceeded 1 MiB")),
        "{:?}",
        result.errors
    );
    assert!(result.errors.iter().any(|e| e.contains("not json at all")));
    Ok(())
}

#[test]
fn cancel_interrupts_a_stuck_subprocess() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (engine, repo) = fake(temp.path(), "exec sleep 60")?;
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(400));
        flag.store(true, Ordering::Relaxed);
    });
    let result = engine.run(&repo, PASSWORD, &["backup".into()], cancel, |_| {})?;
    assert_eq!(result.outcome, Outcome::Cancelled);
    assert!(result.elapsed_seconds < 20.0, "{}", result.elapsed_seconds);
    Ok(())
}
