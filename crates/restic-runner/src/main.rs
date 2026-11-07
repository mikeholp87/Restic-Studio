use anyhow::{Context, Result, bail, ensure};
use restic_core::{Engine, Event, Outcome, Profile, Repository};
use std::{
    io::{self, Read},
    path::Path,
    sync::Arc,
};
fn password() -> Result<String> {
    // stdin is deliberately explicit; no secret command-line or environment values.
    let mut input = String::new();
    io::stdin().take(4097).read_to_string(&mut input)?;
    ensure!(input.len() <= 4096, "Password input is too long");
    while input.ends_with(['\n', '\r']) {
        input.pop();
    }
    ensure!(
        !input.is_empty(),
        "Supply the repository password on stdin (use a protected pipe)"
    );
    Ok(input)
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let engine = Engine::default();
    if args.first().map(String::as_str) == Some("version") {
        println!("{}", engine.version()?);
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("probe") {
        let capabilities = engine.probe();
        println!("{}", serde_json::to_string_pretty(&capabilities)?);
        std::process::exit(if capabilities.ok() { 0 } else { 1 });
    }
    if args.first().map(String::as_str) == Some("device") {
        let path = args.get(1).context("device PATH")?;
        let info = restic_core::ensure_mounted(Path::new(path))?;
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }
    if matches!(
        args.first().map(String::as_str),
        Some("profiles" | "profile-save" | "profile-delete")
    ) {
        let store = restic_core::state_dir()?.join("profiles");
        match args[0].as_str() {
            "profiles" => println!(
                "{}",
                serde_json::to_string_pretty(&restic_core::list_profiles(&store)?)?
            ),
            "profile-save" => {
                let file = args.get(1).context("profile-save PROFILE.json")?;
                let profile: Profile =
                    serde_json::from_reader(io::Read::take(std::fs::File::open(file)?, 1 << 20))?;
                restic_core::save_profile(&profile, &store)?;
                println!("Saved profile {}", profile.id);
            }
            _ => {
                let id = args.get(1).context("profile-delete ID")?;
                restic_core::delete_profile(&store, id)?;
                println!("Deleted profile {id} (repository and snapshots untouched)");
            }
        }
        return Ok(());
    }
    if args.is_empty() {
        println!(
            "Restic Studio runner\nCommands: version | probe | device PATH | profiles | profile-save PROFILE.json | profile-delete ID | init REPO | snapshots REPO | ls REPO SNAPSHOT DIRECTORY | check REPO | preview PROFILE.json | backup PROFILE.json | run-profile ID | restore REPO SNAPSHOT PATH NEW_DIRECTORY\nSupply password on stdin. GTK client: restic-studio-gui."
        );
        return Ok(());
    }
    let secret = password()?;
    let location = || -> Result<Repository> {
        Ok(Repository {
            location: args.get(1).context("Repository missing")?.clone(),
        })
    };
    let result = match args[0].as_str() {
        "init" => engine.init(&location()?, &secret)?,
        "snapshots" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&engine.snapshots(&location()?, &secret)?)?
            );
            return Ok(());
        }
        "ls" => {
            ensure!(args.len() == 4, "ls REPO SNAPSHOT DIRECTORY");
            engine.list_directory(&location()?, &secret, &args[2], &args[3], |entry| {
                if let Ok(line) = serde_json::to_string(&entry) {
                    println!("{line}");
                }
            })?;
            return Ok(());
        }
        "check" => engine.run(
            &location()?,
            &secret,
            &["check".into()],
            Arc::default(),
            |_| {},
        )?,
        "backup" | "run-profile" | "preview" => {
            let argument = args.get(1).context("Profile missing")?;
            let profile: Profile = if args[0] == "run-profile" {
                restic_core::load_profile(&restic_core::state_dir()?.join("profiles"), argument)?
            } else {
                serde_json::from_reader(io::Read::take(std::fs::File::open(argument)?, 1 << 20))?
            };
            let directory = restic_core::state_dir()?;
            let mut progress = |event| {
                if let Event::Json(value) = event
                    && value.get("message_type").and_then(|v| v.as_str()) == Some("status")
                {
                    eprintln!("bytes_done={}", value["bytes_done"]);
                }
            };
            let result = if args[0] == "preview" {
                engine.preview_backup(&profile, &secret, Arc::default(), &mut progress)?
            } else {
                engine.backup(&profile, &secret, Arc::default(), &mut progress)?
            };
            if args[0] != "preview" {
                restic_core::record(&directory, &profile.id, &result)?;
            }
            result
        }
        "restore" => {
            ensure!(args.len() == 5, "restore REPO SNAPSHOT PATH NEW_DIRECTORY");
            engine.restore_new(
                &location()?,
                &secret,
                &args[2],
                &args[3],
                Path::new(&args[4]),
            )?
        }
        _ => bail!("Unknown command"),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    ensure!(
        result.outcome == Outcome::Success,
        "Operation did not fully succeed: {:?}",
        result.outcome
    );
    Ok(())
}
