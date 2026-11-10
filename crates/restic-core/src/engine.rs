use crate::{
    Event, JobResult, Outcome, Profile, Repository, Snapshot, SnapshotEntry, ensure_mounted,
    identity::PinPolicy, snapshot_id, valid_snapshot_path,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, atomic::AtomicBool},
};

/// `restic snapshots --json` prints every snapshot on one line, so it gets a larger cap.
/// 32 MiB is roughly 100,000 snapshots.
const SNAPSHOT_LIMIT: usize = 32 << 20;
/// Oldest Restic with the exit codes 10/11/12 that `classify` relies on.
const MIN_RESTIC: (u32, u32, u32) = (0, 17, 0);
/// Skipped whenever `/` is a backup source: virtual, temporary and mount-point trees.
const ROOT_EXCLUDES: [&str; 9] = [
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/tmp",
    "/var/tmp",
    "/mnt",
    "/media",
    "/lost+found",
];

pub struct Engine {
    pub executable: PathBuf,
    pub cache_dir: Option<PathBuf>,
    /// Where repository identities are pinned; defaults to `state_dir()`.
    pub state_dir: Option<PathBuf>,
}
impl Default for Engine {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("/usr/bin/restic"),
            cache_dir: None,
            state_dir: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Capabilities {
    pub version: Option<String>,
    pub problems: Vec<String>,
}
impl Capabilities {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}
/// "restic 0.19.1 compiled with go1.26.4 on linux/amd64" -> (0, 19, 1)
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split_whitespace().nth(1)?.split('.');
    let mut next = || parts.next()?.parse().ok();
    Some((next()?, next()?, next()?))
}
fn current_hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    std::ffi::CStr::from_bytes_until_nul(&buffer)
        .ok()
        .map(|name| name.to_string_lossy().into_owned())
}

impl Engine {
    pub fn version(&self) -> Result<String> {
        let output = Command::new(&self.executable).arg("version").output()?;
        ensure!(output.status.success(), "Restic version detection failed");
        Ok(String::from_utf8(output.stdout)?.trim().into())
    }
    /// Check the host can run this engine's Restic invocations. Never touches a repository.
    pub fn probe(&self) -> Capabilities {
        let mut problems = Vec::new();
        let version = match self.version() {
            Ok(text) => {
                match parse_version(&text) {
                    Some(found) if found >= MIN_RESTIC => {}
                    Some(_) => problems.push(format!(
                        "Restic {}.{}.{} or newer is required: {text}",
                        MIN_RESTIC.0, MIN_RESTIC.1, MIN_RESTIC.2
                    )),
                    None => problems.push(format!("Unrecognised Restic version output: {text}")),
                }
                Some(text)
            }
            Err(error) => {
                problems.push(format!("Restic unavailable: {error}"));
                None
            }
        };
        // run() pins the repository through /proc/<pid>/fd and feeds the password on /dev/stdin.
        if !Path::new("/proc/self/fd").is_dir() {
            problems.push("/proc is not mounted; repository pinning needs /proc/<pid>/fd".into());
        }
        if !Path::new("/dev/stdin").exists() {
            problems.push("/dev/stdin is missing; the password pipe needs it".into());
        }
        Capabilities { version, problems }
    }
    pub fn init(&self, repo: &Repository, password: &str) -> Result<JobResult> {
        ensure_mounted(Path::new(&repo.location))?;
        let _job = self.lock_repository(repo)?;
        let path = Path::new(&repo.location);
        ensure!(
            !path.exists() || fs::read_dir(path)?.next().is_none(),
            "Initialize only a new or empty directory; use Open for existing repositories"
        );
        self.run(repo, password, &["init".into()], Arc::default(), |_| {})
    }
    /// Restic's structure check, under the same job lock and identity checks as a backup.
    pub fn check(
        &self,
        repo: &Repository,
        password: &str,
        cancel: Arc<AtomicBool>,
        event: impl FnMut(Event),
    ) -> Result<JobResult> {
        let _job = self.lock_repository(repo)?;
        let (_handle, opened) = self.open_verified(repo, password, PinPolicy::Record)?;
        self.run(&opened, password, &["check".into()], cancel, event)
    }
    pub fn backup(
        &self,
        profile: &Profile,
        password: &str,
        cancel: Arc<AtomicBool>,
        event: impl FnMut(Event),
    ) -> Result<JobResult> {
        self.backup_with_mode(profile, password, cancel, event, false)
    }
    pub fn preview_backup(
        &self,
        profile: &Profile,
        password: &str,
        cancel: Arc<AtomicBool>,
        event: impl FnMut(Event),
    ) -> Result<JobResult> {
        self.backup_with_mode(profile, password, cancel, event, true)
    }
    fn backup_with_mode(
        &self,
        profile: &Profile,
        password: &str,
        cancel: Arc<AtomicBool>,
        event: impl FnMut(Event),
        dry_run: bool,
    ) -> Result<JobResult> {
        profile.validate()?;
        ensure!(
            Path::new(&profile.repository.location)
                .join("config")
                .is_file(),
            "Repository unavailable; refusing to create a substitute destination"
        );
        let _job = self.lock_repository(&profile.repository)?;
        let (_handle, opened) =
            self.open_verified(&profile.repository, password, PinPolicy::Record)?;
        let mut args: Vec<OsString> = vec![
            "backup".into(),
            "--tag".into(),
            format!("restic-studio:{}", profile.id).into(),
        ];
        let mut excludes = profile.excludes.clone();
        if profile.sources.iter().any(|source| {
            source
                .canonicalize()
                .is_ok_and(|path| path == Path::new("/"))
        }) {
            excludes.push(
                Path::new(&opened.location)
                    .canonicalize()?
                    .to_string_lossy()
                    .into_owned(),
            );
            excludes.extend(ROOT_EXCLUDES.into_iter().map(str::to_owned));
            if let Some(hostname) = current_hostname()
                && self
                    .list_snapshots(&opened, password)?
                    .iter()
                    .any(|snapshot| {
                        snapshot.hostname == hostname
                            && snapshot
                                .paths
                                .iter()
                                .any(|path| path == "/home" || path.starts_with("/home/"))
                    })
            {
                excludes.push("/home".into());
            }
        }
        if dry_run {
            args.push("--dry-run".into());
        }
        if profile.one_file_system {
            args.push("--one-file-system".into());
        }
        for exclude in &excludes {
            args.extend(["--exclude".into(), exclude.into()]);
        }
        args.push("--".into());
        args.extend(profile.sources.iter().map(|p| p.as_os_str().into()));
        self.run(&opened, password, &args, cancel, event)
    }
    /// Snapshots of `repo`, refused if a different repository was pinned at that location.
    pub fn snapshots(&self, repo: &Repository, password: &str) -> Result<Vec<Snapshot>> {
        let (_handle, opened) = self.open_verified(repo, password, PinPolicy::VerifyKnown)?;
        self.list_snapshots(&opened, password)
    }
    /// Snapshot listing without identity checks; callers must already have verified `repo`.
    fn list_snapshots(&self, repo: &Repository, password: &str) -> Result<Vec<Snapshot>> {
        let mut snapshots = None;
        let result = self.run_limited(
            repo,
            password,
            &["snapshots".into()],
            Arc::default(),
            |event| {
                if let Event::Json(value) = event
                    && let Ok(items) = serde_json::from_value::<Vec<Snapshot>>(value)
                {
                    snapshots = Some(items);
                }
            },
            SNAPSHOT_LIMIT,
        )?;
        ensure!(
            result.outcome == Outcome::Success,
            "Snapshot listing failed: {:?}",
            result.outcome
        );
        snapshots.context(
            "Snapshot response was invalid or exceeded the 32 MiB limit; listing is unavailable",
        )
    }
    /// Read direct children only; callers can navigate without loading an entire tree.
    pub fn list_directory(
        &self,
        repo: &Repository,
        password: &str,
        snapshot: &str,
        directory: &str,
        mut entry: impl FnMut(SnapshotEntry),
    ) -> Result<()> {
        snapshot_id(snapshot)?;
        ensure!(valid_snapshot_path(directory), "Invalid snapshot directory");
        let (_handle, opened) = self.open_verified(repo, password, PinPolicy::VerifyKnown)?;
        let mut invalid = false;
        let result = self.run(
            &opened,
            password,
            &["ls".into(), snapshot.into(), "--".into(), directory.into()],
            Arc::default(),
            |event| match event {
                Event::Json(value) if value["struct_type"] == "node" => {
                    match serde_json::from_value(value) {
                        Ok(node) => entry(node),
                        Err(_) => invalid = true,
                    }
                }
                Event::Diagnostic(_) => invalid = true,
                Event::Json(_) => {}
            },
        )?;
        ensure!(
            result.outcome == Outcome::Success,
            "Snapshot listing failed: {:?}: {}",
            result.outcome,
            result.errors.join("; ")
        );
        ensure!(!invalid, "Snapshot listing was incomplete or malformed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_parsing_and_missing_engine() {
        assert_eq!(
            parse_version("restic 0.19.1 compiled with go1.26.4 on linux/amd64"),
            Some((0, 19, 1))
        );
        assert!((0, 16, 5) < MIN_RESTIC && (0, 17, 0) >= MIN_RESTIC);
        assert_eq!(parse_version("garbage"), None);
        let missing = Engine {
            executable: "/nonexistent/restic".into(),
            ..Engine::default()
        };
        let found = missing.probe();
        assert!(!found.ok() && found.version.is_none());
    }

    #[test]
    fn hostname_is_readable() {
        assert!(current_hostname().is_some_and(|name| !name.is_empty()));
    }
}
