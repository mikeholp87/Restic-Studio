use crate::{
    Engine, Event, Outcome, Repository, ensure_mounted, fd_path, has_parent_traversal,
    model::resolved_destination, mounts::drive_id, open_directory, snapshot_id, state_dir,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

/// Held for the length of a job on one repository; released on drop (or process exit).
#[derive(Debug)]
pub struct JobLock(#[allow(dead_code)] fs::File);

/// Whether an identity check may create or update a pin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PinPolicy {
    /// Trust on first use: record the identity when none is pinned yet.
    Record,
    /// Read-only access: compare against an existing pin, never write one.
    VerifyKnown,
}

const PINS_FILE: &str = "repository-ids.json";

/// FNV-1a: a stable name for a lock file, so every client and version derives the same one.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}
fn flock(file: &fs::File, operation: i32) -> bool {
    use std::os::fd::AsRawFd;
    unsafe { libc::flock(file.as_raw_fd(), operation) == 0 }
}
fn open_lock(directory: &Path, name: &str) -> Result<fs::File> {
    Ok(OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join(name))?)
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RepositoryPin {
    Legacy(String),
    Current { id: String, drive: Option<String> },
}
fn read_pins(file: &Path) -> Result<BTreeMap<String, RepositoryPin>> {
    match fs::read(file) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("repository-ids.json is corrupt")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
}

impl Engine {
    /// Restic's own repository ID, read from the decrypted `config`.
    pub fn repository_id(&self, repo: &Repository, password: &str) -> Result<String> {
        let mut text = String::new();
        let result = self.run(
            repo,
            password,
            &["cat".into(), "config".into()],
            Arc::default(),
            |event| match event {
                Event::Diagnostic(line) => text.push_str(&line),
                Event::Json(value) => text.push_str(&value.to_string()),
            },
        )?;
        ensure!(
            result.outcome == Outcome::Success,
            "Repository identity unavailable: {:?}",
            result.outcome
        );
        // run() redacts the password from output; a short password could corrupt the ID.
        ensure!(
            !text.contains("[REDACTED]"),
            "Cannot read repository identity: the password appears in the ID"
        );
        let id = serde_json::from_str::<Value>(&text)?
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("Repository config has no ID")?;
        ensure!(snapshot_id(&id).is_ok(), "Repository ID is malformed");
        Ok(id)
    }
    pub(crate) fn state(&self) -> Result<PathBuf> {
        match &self.state_dir {
            Some(path) => {
                fs::create_dir_all(path)?;
                Ok(path.clone())
            }
            None => state_dir(),
        }
    }
    /// Claim one repository for a job so the GUI, runner and other instances cannot overlap.
    /// Fails at once, rather than waiting, if another job holds it.
    pub fn lock_repository(&self, repo: &Repository) -> Result<JobLock> {
        let key = resolved_destination(Path::new(&repo.location))?;
        let name = format!(
            "job-{:016x}.lock",
            fnv1a(key.as_os_str().as_encoded_bytes())
        );
        let file = open_lock(&self.state()?, &name)?;
        ensure!(
            flock(&file, libc::LOCK_EX | libc::LOCK_NB),
            "Another Restic Studio job is already using this repository"
        );
        Ok(JobLock(file))
    }
    /// Trust on first use: pin the repository's ID per path, refuse a different one later.
    pub fn verify_identity(&self, repo: &Repository, password: &str) -> Result<()> {
        self.verify_identity_at(repo, repo, password, PinPolicy::Record)
    }
    pub(crate) fn verify_identity_at(
        &self,
        repo: &Repository,
        opened: &Repository,
        password: &str,
        policy: PinPolicy,
    ) -> Result<()> {
        self.verify_identity_with_drive(
            repo,
            opened,
            password,
            drive_id(Path::new(&opened.location))?,
            policy,
        )
    }
    fn verify_identity_with_drive(
        &self,
        repo: &Repository,
        opened: &Repository,
        password: &str,
        drive: Option<String>,
        policy: PinPolicy,
    ) -> Result<()> {
        let key = resolved_destination(Path::new(&repo.location))?
            .to_string_lossy()
            .into_owned();
        let directory = self.state()?;
        let file = directory.join(PINS_FILE);
        // Pins for different repositories share one file: serialise the read-modify-write.
        // Readers need no lock because writers replace the file atomically.
        let _pin_lock = match policy {
            PinPolicy::Record => {
                let lock = open_lock(&directory, "pins.lock")?;
                ensure!(
                    flock(&lock, libc::LOCK_EX),
                    "Could not lock the identity pins"
                );
                Some(lock)
            }
            PinPolicy::VerifyKnown => None,
        };
        let mut pins = read_pins(&file)?;
        if policy == PinPolicy::VerifyKnown && !pins.contains_key(&key) {
            return Ok(());
        }
        let actual = self.repository_id(opened, password)?;
        match pins.get(&key) {
            Some(RepositoryPin::Legacy(id) | RepositoryPin::Current { id, .. })
                if *id != actual =>
            {
                bail!(
                    "The repository at this location is not the one used before (identity changed). \
                 If you replaced it on purpose, remove its entry from {}",
                    file.display()
                )
            }
            Some(RepositoryPin::Current {
                drive: Some(pinned),
                ..
            }) => {
                ensure!(
                    drive.as_ref() == Some(pinned),
                    "The repository drive identity changed or is unavailable. Reconnect the original drive; if it was intentionally replaced, remove its entry from {}",
                    file.display()
                );
                return Ok(());
            }
            Some(RepositoryPin::Current { drive: None, .. }) if drive.is_none() => return Ok(()),
            _ => {}
        }
        if policy == PinPolicy::VerifyKnown {
            return Ok(());
        }
        pins.insert(key, RepositoryPin::Current { id: actual, drive });
        let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut temp, &pins)?;
        temp.as_file().sync_all()?;
        temp.persist(file)?;
        Ok(())
    }
    /// Open the repository by handle so a later path swap cannot redirect the job.
    pub(crate) fn pin_repository(&self, repo: &Repository) -> Result<(fs::File, Repository)> {
        let path = Path::new(&repo.location);
        ensure!(
            path.is_absolute(),
            "Only local absolute repositories are supported"
        );
        ensure!(
            !has_parent_traversal(path),
            "Repository path may not contain parent traversal"
        );
        ensure_mounted(path)?;
        let handle = open_directory(path, 0)?;
        let opened = Repository {
            location: fd_path(&handle).to_string_lossy().into_owned(),
        };
        Ok((handle, opened))
    }
    /// Pin the repository and check it is the one recorded for its location.
    /// Keep the returned handle alive while the pinned location is in use.
    pub(crate) fn open_verified(
        &self,
        repo: &Repository,
        password: &str,
        policy: PinPolicy,
    ) -> Result<(fs::File, Repository)> {
        let (handle, opened) = self.pin_repository(repo)?;
        self.verify_identity_at(repo, &opened, password, policy)?;
        Ok((handle, opened))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(temp: &Path) -> Engine {
        Engine {
            state_dir: Some(temp.join("state")),
            cache_dir: Some(temp.join("cache")),
            ..Engine::default()
        }
    }
    fn repository(path: PathBuf) -> Repository {
        Repository {
            location: path.to_string_lossy().into_owned(),
        }
    }

    #[test]
    fn pinned_repository_survives_path_replacement() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let engine = engine(temp.path());
        let repo = repository(temp.path().join("repo"));
        assert!(
            engine
                .pin_repository(&repository("relative".into()))
                .is_err()
        );
        assert!(
            engine
                .pin_repository(&repository("/tmp/../tmp".into()))
                .is_err()
        );
        let replacement = repository(temp.path().join("replacement"));
        engine.init(&repo, "password")?;
        engine.init(&replacement, "password")?;
        let (handle, opened) = engine.pin_repository(&repo)?;
        engine.verify_identity_at(&repo, &opened, "password", PinPolicy::Record)?;
        let original_id = engine.repository_id(&opened, "password")?;
        fs::rename(&repo.location, temp.path().join("original"))?;
        fs::rename(&replacement.location, &repo.location)?;
        assert_ne!(engine.repository_id(&repo, "password")?, original_id);
        assert_eq!(engine.repository_id(&opened, "password")?, original_id);
        engine.verify_identity_at(&repo, &opened, "password", PinPolicy::Record)?;
        assert!(engine.verify_identity(&repo, "password").is_err());
        drop(handle);
        Ok(())
    }

    #[test]
    fn drive_identity_migrates_legacy_pin_and_rejects_swap() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let engine = engine(temp.path());
        let repo = repository(temp.path().join("repo"));
        engine.init(&repo, "password")?;
        let id = engine.repository_id(&repo, "password")?;
        fs::write(
            temp.path().join("state").join(PINS_FILE),
            serde_json::to_vec(&serde_json::json!({ &repo.location: id }))?,
        )?;
        let check = |drive: Option<&str>| {
            engine.verify_identity_with_drive(
                &repo,
                &repo,
                "password",
                drive.map(str::to_owned),
                PinPolicy::Record,
            )
        };
        check(Some("drive-a"))?;
        check(Some("drive-a"))?;
        assert!(check(Some("drive-b")).is_err());
        assert!(check(None).is_err());
        Ok(())
    }

    #[test]
    fn verify_known_never_records_a_pin() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let engine = engine(temp.path());
        let (repo, other) = (
            repository(temp.path().join("repo")),
            repository(temp.path().join("other")),
        );
        engine.init(&repo, "password")?;
        engine.init(&other, "password")?;
        engine.verify_identity_at(&repo, &repo, "password", PinPolicy::VerifyKnown)?;
        assert!(!temp.path().join("state").join(PINS_FILE).exists());
        engine.verify_identity(&repo, "password")?;
        engine.verify_identity_at(&repo, &repo, "password", PinPolicy::VerifyKnown)?;
        assert!(
            engine
                .verify_identity_at(&repo, &other, "password", PinPolicy::VerifyKnown)
                .is_err()
        );
        Ok(())
    }
}
