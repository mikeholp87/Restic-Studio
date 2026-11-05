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
    if args.is_empty() {
        println!(
            "Restic Studio runner\nCommands: version | init REPO | snapshots REPO | ls REPO SNAPSHOT DIRECTORY | check REPO | backup PROFILE.json | run-profile ID | restore REPO SNAPSHOT PATH NEW_DIRECTORY\nSupply password on stdin. GTK client: restic-studio-gui."
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
        "backup" | "run-profile" => {
            let argument = args.get(1).context("Profile missing")?;
            let path = if args[0] == "run-profile" {
                restic_core::valid_id(argument)?;
                restic_core::state_dir()?
                    .join("profiles")
                    .join(format!("{argument}.json"))
            } else {
                argument.into()
            };
            let profile: Profile = serde_json::from_reader(std::fs::File::open(path)?)?;
            let directory = restic_core::state_dir()?;
            let lock = std::fs::File::create(directory.join("job.lock"))?;
            use std::os::fd::AsRawFd;
            ensure!(
                unsafe { libc_flock(lock.as_raw_fd()) },
                "Another Studio job is active"
            );
            let result = engine.backup(&profile, &secret, Arc::default(), |event| {
                if let Event::Json(value) = event
                    && value.get("message_type").and_then(|v| v.as_str()) == Some("status")
                {
                    eprintln!("bytes_done={}", value["bytes_done"]);
                }
            })?;
            restic_core::record(&directory, &profile.id, &result)?;
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
unsafe fn libc_flock(fd: i32) -> bool {
    unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0 }
}
