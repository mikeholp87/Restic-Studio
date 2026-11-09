//! Restic CLI boundary. No shell, implicit unlock, or destructive defaults.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repository {
    pub location: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub repository: Repository,
    pub sources: Vec<PathBuf>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub one_file_system: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub time: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub tags: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub size: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Outcome {
    Success,
    Partial,
    Cancelled,
    MissingRepository,
    Locked,
    WrongPassword,
    Failed,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct JobResult {
    pub outcome: Outcome,
    pub exit_code: Option<i32>,
    pub snapshot_id: Option<String>,
    pub elapsed_seconds: f64,
    pub errors: Vec<String>,
}
#[derive(Debug)]
pub enum Event {
    Json(Value),
    Diagnostic(String),
}

/// Largest single line of Restic output accepted by `run`.
const RECORD_LIMIT: usize = 1 << 20;
/// `restic snapshots --json` prints every snapshot on one line, so it gets a larger cap.
/// 32 MiB is roughly 100,000 snapshots.
const SNAPSHOT_LIMIT: usize = 32 << 20;
/// Oldest Restic with the exit codes 10/11/12 that `classify` relies on.
const MIN_RESTIC: (u32, u32, u32) = (0, 17, 0);
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

/// Held for the length of a job on one repository; released on drop (or process exit).
#[derive(Debug)]
pub struct JobLock(#[allow(dead_code)] fs::File);
/// FNV-1a: a stable name for a lock file, so every client and version derives the same one.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
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

fn udev_drive_id(data: &str) -> Option<String> {
    ["E:ID_WWN_WITH_EXTENSION=", "E:ID_WWN=", "E:ID_SERIAL="]
        .iter()
        .find_map(|prefix| {
            data.lines()
                .find_map(|line| line.strip_prefix(prefix).filter(|value| !value.is_empty()))
        })
        .map(str::to_owned)
}

fn drive_id(path: &Path) -> Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    let device = fs::metadata(path)?.dev();
    let data = format!(
        "/run/udev/data/b{}:{}",
        libc::major(device),
        libc::minor(device)
    );
    match fs::read_to_string(data) {
        Ok(data) => Ok(udev_drive_id(&data)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Serialize)]
pub struct MountInfo {
    pub mount_point: PathBuf,
    pub fstype: String,
    pub source: String,
    /// Kernel's removable flag for the underlying disk; None when sysfs does not say.
    pub removable: Option<bool>,
}
/// Longest mount point containing `path`, from a /proc/self/mountinfo table.
fn parse_mount(table: &str, path: &Path) -> Option<MountInfo> {
    let mut best: Option<(MountInfo, String)> = None;
    for line in table.lines() {
        let (head, tail) = line.split_once(" - ")?;
        let fields: Vec<&str> = head.split(' ').collect();
        let mut after = tail.split(' ');
        let (fstype, source) = (after.next()?, after.next()?);
        // mountinfo escapes spaces and tabs as octal.
        let mount_point =
            PathBuf::from(fields.get(4)?.replace("\\040", " ").replace("\\011", "\t"));
        if path.starts_with(&mount_point)
            && best.as_ref().is_none_or(|(b, _)| {
                mount_point.as_os_str().len() >= b.mount_point.as_os_str().len()
            })
        {
            let info = MountInfo {
                mount_point,
                fstype: fstype.into(),
                source: source.into(),
                removable: None,
            };
            best = Some((info, (*fields.get(2)?).to_owned()));
        }
    }
    best.map(|(mut info, majmin)| {
        // Partitions keep the flag on their parent disk.
        info.removable = ["", "/.."].iter().find_map(|up| {
            fs::read_to_string(format!("/sys/dev/block/{majmin}{up}/removable"))
                .ok()
                .map(|v| v.trim() == "1")
        });
        info
    })
}
/// Mount holding `path`, or its nearest existing ancestor if it does not exist yet.
pub fn mount_info(path: &Path) -> Result<MountInfo> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent().context("No existing parent directory")?;
    }
    let real = existing.canonicalize()?;
    parse_mount(&fs::read_to_string("/proc/self/mountinfo")?, &real)
        .context("Could not find the mount for this location")
}
/// True when a usual drive location resolves outside its mount directory.
fn unmounted_drive_location(info: &MountInfo, path: &Path) -> bool {
    ["/media", "/mnt", "/run/media"]
        .iter()
        .any(|dir| path.starts_with(dir) && !info.mount_point.starts_with(dir))
}
pub fn ensure_mounted(path: &Path) -> Result<MountInfo> {
    let info = mount_info(path)?;
    let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    ensure!(
        !unmounted_drive_location(&info, path) && !unmounted_drive_location(&info, &real),
        "{} is under a drive mount directory but on the system disk; the drive does not look mounted. Connect and mount it, then select it again",
        path.display()
    );
    Ok(info)
}

pub fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "Invalid identifier"
    );
    Ok(())
}
pub fn snapshot_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64 && id.bytes().all(|c| c.is_ascii_hexdigit()),
        "A full snapshot ID is required"
    );
    Ok(())
}
fn current_hostname() -> Option<String> {
    let mut hostname = [0i8; 256];
    if unsafe { libc::gethostname(hostname.as_mut_ptr(), hostname.len()) } != 0 {
        return None;
    }
    Some(
        unsafe { std::ffi::CStr::from_ptr(hostname.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}
impl Profile {
    pub fn validate(&self) -> Result<()> {
        valid_id(&self.id)?;
        ensure!(!self.name.trim().is_empty(), "Profile name is empty");
        ensure!(!self.sources.is_empty(), "Select at least one source");
        ensure!(
            Path::new(&self.repository.location).is_absolute(),
            "This release supports absolute local repository paths only"
        );
        let destination = resolved_destination(Path::new(&self.repository.location))?;
        for source in &self.sources {
            ensure!(source.is_absolute(), "Source paths must be absolute");
            let source = source.canonicalize().context("Source unavailable")?;
            let root_backup = source == Path::new("/") && destination != Path::new("/");
            ensure!(
                root_backup
                    || (!destination.starts_with(&source) && !source.starts_with(&destination)),
                "Sources and repository must not contain one another"
            );
        }
        ensure!(
            self.excludes.iter().all(|s| !s.contains(['\n', '\0'])),
            "Invalid exclusion"
        );
        Ok(())
    }
}
fn resolved_destination(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "Parent traversal is forbidden"
    );
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let parent = path.parent().context("Missing parent")?.canonicalize()?;
    Ok(parent.join(path.file_name().context("Missing filename")?))
}
pub fn classify(code: Option<i32>, cancelled: bool, backup: bool) -> Outcome {
    if cancelled {
        return Outcome::Cancelled;
    }
    match code {
        Some(0) => Outcome::Success,
        Some(3) if backup => Outcome::Partial,
        Some(10) => Outcome::MissingRepository,
        Some(11) => Outcome::Locked,
        Some(12) => Outcome::WrongPassword,
        _ => Outcome::Failed,
    }
}

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
            Ok(text) => match parse_version(&text) {
                Some(found) if found >= MIN_RESTIC => Some(text),
                Some(_) => {
                    problems.push(format!(
                        "Restic {}.{}.{} or newer is required: {text}",
                        MIN_RESTIC.0, MIN_RESTIC.1, MIN_RESTIC.2
                    ));
                    Some(text)
                }
                None => {
                    problems.push(format!("Unrecognised Restic version output: {text}"));
                    Some(text)
                }
            },
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
    fn state(&self) -> Result<PathBuf> {
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
        self.verify_identity_at(repo, repo, password)
    }
    fn verify_identity_at(
        &self,
        repo: &Repository,
        opened: &Repository,
        password: &str,
    ) -> Result<()> {
        self.verify_identity_with_drive(
            repo,
            opened,
            password,
            drive_id(Path::new(&opened.location))?,
        )
    }
    fn verify_identity_with_drive(
        &self,
        repo: &Repository,
        opened: &Repository,
        password: &str,
        drive: Option<String>,
    ) -> Result<()> {
        let key = resolved_destination(Path::new(&repo.location))?
            .to_string_lossy()
            .into_owned();
        let directory = self.state()?;
        // Pins for different repositories share one file: serialise the read-modify-write.
        let pin_lock = open_lock(&directory, "pins.lock")?;
        ensure!(
            flock(&pin_lock, libc::LOCK_EX),
            "Could not lock the identity pins"
        );
        let file = directory.join("repository-ids.json");
        let mut pins: std::collections::BTreeMap<String, RepositoryPin> = match fs::read(&file) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("repository-ids.json is corrupt")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(error) => return Err(error.into()),
        };
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
        pins.insert(key, RepositoryPin::Current { id: actual, drive });
        fs::create_dir_all(&directory)?;
        let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer_pretty(&mut temp, &pins)?;
        temp.as_file().sync_all()?;
        temp.persist(file)?;
        Ok(())
    }
    fn pin_repository(&self, repo: &Repository) -> Result<(fs::File, Repository)> {
        use std::os::fd::AsRawFd;
        let path = Path::new(&repo.location);
        ensure!(
            path.is_absolute(),
            "Only local absolute repositories are supported"
        );
        ensure!(
            !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
            "Repository path may not contain parent traversal"
        );
        ensure_mounted(path)?;
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(path)?;
        let opened = Repository {
            location: format!("/proc/{}/fd/{}", std::process::id(), handle.as_raw_fd()),
        };
        Ok((handle, opened))
    }
    pub fn run(
        &self,
        repo: &Repository,
        password: &str,
        args: &[OsString],
        cancel: Arc<AtomicBool>,
        event: impl FnMut(Event),
    ) -> Result<JobResult> {
        self.run_limited(repo, password, args, cancel, event, RECORD_LIMIT)
    }
    /// Like `run`, but each output record (one line of Restic output) may be up to `max_record` bytes.
    pub fn run_limited(
        &self,
        repo: &Repository,
        password: &str,
        args: &[OsString],
        cancel: Arc<AtomicBool>,
        mut event: impl FnMut(Event),
        max_record: usize,
    ) -> Result<JobResult> {
        ensure!(
            !password.contains(['\n', '\r']),
            "Password must be a single line"
        );
        ensure!(
            !password.is_empty() && password.len() <= 4096,
            "Password must contain 1–4096 bytes"
        );
        ensure!(
            Path::new(&repo.location).is_absolute(),
            "Only local absolute repositories are supported"
        );
        use std::os::fd::AsRawFd;
        let location = Path::new(&repo.location);
        ensure!(
            !location
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
            "Repository path may not contain parent traversal"
        );
        let (pinned, suffix) = if location.exists() {
            (
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY)
                    .open(location)?,
                None,
            )
        } else {
            let parent = location.parent().context("Repository parent missing")?;
            (
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY)
                    .open(parent)?,
                Some(location.file_name().context("Repository name missing")?),
            )
        };
        let mut pinned_location = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            pinned.as_raw_fd()
        ));
        if let Some(name) = suffix {
            pinned_location.push(name);
        }
        let started = Instant::now();
        let mut command = Command::new(&self.executable);
        if let Some(cache) = &self.cache_dir {
            command.arg("--cache-dir").arg(cache);
        }
        // Ignore inherited Restic overrides, especially password-command and no-lock settings.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("RESTIC_") {
                command.env_remove(key);
            }
        }
        command
            .args(["--json", "--repo"])
            .arg(&pinned_location)
            .args(["--password-file", "/dev/stdin"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().context("Could not start Restic")?;
        let mut input = child.stdin.take().context("Missing password pipe")?;
        let password_result = writeln!(input, "{password}");
        drop(input);
        let (tx, rx) = mpsc::sync_channel(128);
        let stdout = child.stdout.take().context("Missing stdout")?;
        let stderr = child.stderr.take().context("Missing stderr")?;
        let read = |stream: Box<dyn std::io::Read + Send>,
                    tx: mpsc::SyncSender<std::result::Result<Value, String>>| {
            thread::spawn(move || {
                // Bound individual records as well as the queue. Oversized records are drained.
                let mut reader = BufReader::new(stream);
                loop {
                    let mut bytes = Vec::new();
                    let mut oversized = false;
                    loop {
                        let buffer = match reader.fill_buf() {
                            Ok(b) => b,
                            Err(error) => {
                                let _ =
                                    tx.send(Err(format!("Process output read failed: {error}")));
                                return;
                            }
                        };
                        if buffer.is_empty() {
                            break;
                        }
                        let n = buffer
                            .iter()
                            .position(|b| *b == b'\n')
                            .map_or(buffer.len(), |i| i + 1);
                        let end = buffer[n - 1] == b'\n';
                        if bytes.len() + n <= max_record {
                            bytes.extend_from_slice(&buffer[..n]);
                        } else {
                            oversized = true;
                        }
                        reader.consume(n);
                        if end {
                            break;
                        }
                    }
                    if bytes.is_empty() && !oversized {
                        return;
                    }
                    let value = if oversized {
                        Err(format!(
                            "Process output record exceeded {} MiB",
                            max_record >> 20
                        ))
                    } else {
                        serde_json::from_slice(&bytes).map_err(|_| {
                            String::from_utf8_lossy(&bytes).chars().take(2048).collect()
                        })
                    };
                    if tx.send(value).is_err() {
                        return;
                    }
                }
            })
        };
        let out = read(Box::new(stdout), tx.clone());
        let err = read(Box::new(stderr), tx.clone());
        drop(tx);
        let mut snapshot = None;
        let mut errors = Vec::new();
        let mut interrupted = false;
        loop {
            if cancel.load(Ordering::Relaxed) && !interrupted {
                // SIGINT asks Restic to finish cleanup; forced termination is deliberately not automatic.
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGINT);
                }
                interrupted = true;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(value)) => {
                    if value["message_type"] == "exit_error" || value["message_type"] == "error" {
                        let message = value.get("message").and_then(Value::as_str).or_else(|| {
                            value
                                .get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(Value::as_str)
                        });
                        if let Some(message) = message
                            && errors.len() < 8
                        {
                            errors.push(
                                message
                                    .chars()
                                    .take(2048)
                                    .collect::<String>()
                                    .replace(password, "[REDACTED]")
                                    .replace(&repo.location, "[repository]"),
                            );
                        }
                    }
                    if let Some(id) = value.get("snapshot_id").and_then(Value::as_str) {
                        snapshot = Some(id.into());
                    }
                    event(Event::Json(value));
                }
                Ok(Err(message)) => {
                    let message = message
                        .replace(password, "[REDACTED]")
                        .replace(&repo.location, "[repository]");
                    if !message.trim().is_empty() && errors.len() < 8 {
                        errors.push(message.clone());
                    }
                    event(Event::Diagnostic(message));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = child.wait()?;
        out.join()
            .map_err(|_| anyhow::anyhow!("stdout reader failed"))?;
        err.join()
            .map_err(|_| anyhow::anyhow!("stderr reader failed"))?;
        if status.success() {
            password_result?;
        }
        let backup = args.first().is_some_and(|s| s == "backup");
        Ok(JobResult {
            outcome: classify(status.code(), interrupted, backup),
            exit_code: status.code(),
            snapshot_id: snapshot,
            elapsed_seconds: started.elapsed().as_secs_f64(),
            errors,
        })
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
        let (_repository_handle, opened) = self.pin_repository(&profile.repository)?;
        self.verify_identity_at(&profile.repository, &opened, password)?;
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
            excludes.extend(
                [
                    "/dev",
                    "/proc",
                    "/sys",
                    "/run",
                    "/tmp",
                    "/var/tmp",
                    "/mnt",
                    "/media",
                    "/lost+found",
                ]
                .into_iter()
                .map(str::to_owned),
            );
            if let Some(hostname) = current_hostname()
                && self.snapshots(&opened, password)?.iter().any(|snapshot| {
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
    pub fn snapshots(&self, repo: &Repository, password: &str) -> Result<Vec<Snapshot>> {
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
        ensure!(
            directory.starts_with('/')
                && !directory.contains('\0')
                && !directory.split('/').any(|part| part == ".."),
            "Invalid snapshot directory"
        );
        let mut invalid = false;
        let result = self.run(
            repo,
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
                _ => {}
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
    pub fn restore_new(
        &self,
        repo: &Repository,
        password: &str,
        snapshot: &str,
        selected: &str,
        destination: &Path,
    ) -> Result<JobResult> {
        self.restore_new_with_progress(
            repo,
            password,
            snapshot,
            selected,
            destination,
            Arc::default(),
            |_| {},
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "restore selection and cancellation are separate safety inputs"
    )]
    pub fn restore_new_with_progress(
        &self,
        repo: &Repository,
        password: &str,
        snapshot: &str,
        selected: &str,
        destination: &Path,
        cancel: Arc<AtomicBool>,
        event: impl FnMut(Event),
    ) -> Result<JobResult> {
        snapshot_id(snapshot)?;
        let _job = self.lock_repository(repo)?;
        let (_repository_handle, opened) = self.pin_repository(repo)?;
        self.verify_identity_at(repo, &opened, password)?;
        ensure!(
            selected.starts_with('/')
                && !selected.contains('\0')
                && !selected.split('/').any(|s| s == ".."),
            "Invalid snapshot path"
        );
        ensure!(
            destination.is_absolute() && !destination.exists(),
            "Restore destination must be a new absolute directory"
        );
        let parent = destination
            .parent()
            .context("Missing destination parent")?
            .canonicalize()?;
        let name = destination
            .file_name()
            .context("Missing destination name")?;
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        let parent_handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&parent)?;
        let metadata = parent_handle.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "Restore parent must be owned by you and not writable by other users (owner {}, effective {}, mode {:o})",
            metadata.uid(),
            unsafe { libc::geteuid() },
            metadata.mode()
        );
        let pinned_parent = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            parent_handle.as_raw_fd()
        ));
        ensure!(
            parent != Path::new("/")
                && !parent.starts_with("/etc")
                && !parent.starts_with("/usr")
                && !parent.starts_with("/boot"),
            "Unsafe restore location"
        );
        let staging = tempfile::Builder::new()
            .prefix(".restic-studio-restore-")
            .tempdir_in(&pinned_parent)?;
        fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))?;
        let selected_parent = Path::new(selected)
            .parent()
            .context("Select a file or subdirectory, not the snapshot root")?;
        let selected_name = Path::new(selected)
            .file_name()
            .context("Select a named file or directory")?
            .to_str()
            .context("Invalid filename")?;
        let restore_target = staging.path().join(selected_parent.strip_prefix("/")?);
        fs::create_dir_all(&restore_target)?;
        let escaped: String = format!("/{selected_name}")
            .chars()
            .flat_map(|c| {
                if "*?[]\\".contains(c) {
                    vec!['\\', c]
                } else {
                    vec![c]
                }
            })
            .collect();
        let args = vec![
            "restore".into(),
            format!("{}:{}", snapshot, selected_parent.display()).into(),
            "--target".into(),
            restore_target.as_os_str().into(),
            "--include".into(),
            escaped.into(),
            "--overwrite".into(),
            "never".into(),
            "--verify".into(),
        ];
        let mut result = self.run(&opened, password, &args, cancel.clone(), event)?;
        if cancel.load(Ordering::Relaxed) || result.outcome == Outcome::Cancelled {
            result.outcome = Outcome::Cancelled;
            return Ok(result);
        }
        if result.outcome != Outcome::Success {
            bail!(
                "Restore failed: {:?}; destination unchanged",
                result.outcome
            );
        }
        ensure!(
            staging
                .path()
                .join(selected.trim_start_matches('/'))
                .symlink_metadata()
                .is_ok(),
            "Selected path was not restored"
        );
        let old = std::ffi::CString::new(
            staging
                .path()
                .file_name()
                .context("Missing staging name")?
                .as_encoded_bytes(),
        )?;
        let new = std::ffi::CString::new(name.as_encoded_bytes())?;
        // Atomic no-replace publication prevents races from overwriting an existing destination.
        let rc = unsafe {
            libc::renameat2(
                parent_handle.as_raw_fd(),
                old.as_ptr(),
                parent_handle.as_raw_fd(),
                new.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc != 0 {
            let kept = staging.keep();
            let display_path = parent.join(kept.file_name().context("Missing staging name")?);
            bail!(
                "Could not publish restore: {}; staged data retained under the destination parent, originally {}",
                std::io::Error::last_os_error(),
                display_path.display()
            );
        }
        Ok(result)
    }
}
pub fn state_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .context("No XDG state directory or home")?;
    ensure!(base.is_absolute(), "XDG state directory must be absolute");
    let path = base.join("restic-studio");
    fs::create_dir_all(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}
pub fn save_profile(profile: &Profile, directory: &Path) -> Result<()> {
    profile.validate()?;
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut temp, profile)?;
    temp.as_file().sync_all()?;
    temp.persist(directory.join(format!("{}.json", profile.id)))?;
    Ok(())
}
/// Profiles live as `<id>.json` files in `directory`; the filename is the identity.
const PROFILE_LIMIT: u64 = 1 << 20;
pub fn load_profile(directory: &Path, id: &str) -> Result<Profile> {
    valid_id(id)?;
    let file = fs::File::open(directory.join(format!("{id}.json")))
        .with_context(|| format!("Profile {id} not found"))?;
    let profile: Profile = serde_json::from_reader(std::io::Read::take(file, PROFILE_LIMIT))
        .with_context(|| format!("Profile {id} is unreadable"))?;
    ensure!(
        profile.id == id,
        "Profile file {id}.json holds a different profile ID"
    );
    Ok(profile)
}
#[derive(Debug, Serialize)]
pub struct ProfileList {
    pub profiles: Vec<Profile>,
    /// Files that could not be used; one bad file never hides the others.
    pub problems: Vec<String>,
}
/// Lenient on purpose: a profile whose drive is absent still lists. `validate` runs on use.
pub fn list_profiles(directory: &Path) -> Result<ProfileList> {
    let mut list = ProfileList {
        profiles: Vec::new(),
        problems: Vec::new(),
    };
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(list),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        match load_profile(directory, stem) {
            Ok(profile) => list.profiles.push(profile),
            Err(error) => list.problems.push(format!("{}: {error:#}", path.display())),
        }
    }
    list.profiles.sort_by_key(|p| p.name.to_lowercase());
    list.problems.sort();
    Ok(list)
}
/// Removes only the profile file; repositories, snapshots and pins are untouched.
pub fn delete_profile(directory: &Path, id: &str) -> Result<()> {
    valid_id(id)?;
    fs::remove_file(directory.join(format!("{id}.json")))
        .with_context(|| format!("Profile {id} not found"))
}
pub fn record(directory: &Path, profile: &str, result: &JobResult) -> Result<()> {
    valid_id(profile)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(directory.join("activity.jsonl"))?;
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({"profile":profile,"time":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),"result":result}),
    )?;
    writeln!(file)?;
    file.sync_data()?;
    Ok(())
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
    fn mount_lookup_and_unmounted_drive_detection() {
        let table = "22 1 259:2 / / rw - ext4 /dev/nvme0n1p2 rw\n\
            90 22 8:17 / /media/mike/My\\040Disk rw shared:1 - exfat /dev/sdb1 rw\n";
        let root = parse_mount(table, Path::new("/home/mike/repo")).unwrap();
        assert_eq!(root.mount_point, Path::new("/"));
        let drive = parse_mount(table, Path::new("/media/mike/My Disk/backup")).unwrap();
        assert_eq!(
            (drive.fstype.as_str(), drive.source.as_str()),
            ("exfat", "/dev/sdb1")
        );
        // Same path, drive absent: resolves to / and must be refused.
        assert!(unmounted_drive_location(
            &root,
            Path::new("/media/mike/My Disk/backup")
        ));
        assert!(!unmounted_drive_location(
            &drive,
            Path::new("/media/mike/My Disk/backup")
        ));
        assert!(!unmounted_drive_location(
            &root,
            Path::new("/home/mike/repo")
        ));
        let run = parse_mount(
            "22 1 0:1 / /run rw - tmpfs tmpfs rw\n",
            Path::new("/run/media/user/disk"),
        )
        .unwrap();
        assert!(unmounted_drive_location(
            &run,
            Path::new("/run/media/user/disk")
        ));
        assert!(ensure_mounted(Path::new("/proc")).is_ok());
        assert!(ensure_mounted(Path::new("/media/definitely-not-mounted/x")).is_err());
    }

    #[test]
    fn pinned_repository_survives_path_replacement() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let engine = Engine {
            state_dir: Some(temp.path().join("state")),
            cache_dir: Some(temp.path().join("cache")),
            ..Engine::default()
        };
        let repo = Repository {
            location: temp.path().join("repo").to_string_lossy().into_owned(),
        };
        assert!(
            engine
                .pin_repository(&Repository {
                    location: "relative".into()
                })
                .is_err()
        );
        assert!(
            engine
                .pin_repository(&Repository {
                    location: "/tmp/../tmp".into()
                })
                .is_err()
        );
        let replacement = Repository {
            location: temp
                .path()
                .join("replacement")
                .to_string_lossy()
                .into_owned(),
        };
        engine.init(&repo, "password")?;
        engine.init(&replacement, "password")?;
        let (handle, opened) = engine.pin_repository(&repo)?;
        engine.verify_identity_at(&repo, &opened, "password")?;
        let original_id = engine.repository_id(&opened, "password")?;
        fs::rename(&repo.location, temp.path().join("original"))?;
        fs::rename(&replacement.location, &repo.location)?;
        assert_ne!(engine.repository_id(&repo, "password")?, original_id);
        assert_eq!(engine.repository_id(&opened, "password")?, original_id);
        engine.verify_identity_at(&repo, &opened, "password")?;
        assert!(engine.verify_identity(&repo, "password").is_err());
        drop(handle);
        Ok(())
    }

    #[test]
    fn drive_identity_migrates_legacy_pin_and_rejects_swap() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let engine = Engine {
            state_dir: Some(temp.path().join("state")),
            cache_dir: Some(temp.path().join("cache")),
            ..Engine::default()
        };
        let repo = Repository {
            location: temp.path().join("repo").to_string_lossy().into_owned(),
        };
        engine.init(&repo, "password")?;
        let id = engine.repository_id(&repo, "password")?;
        fs::write(
            temp.path().join("state/repository-ids.json"),
            serde_json::to_vec(&serde_json::json!({ &repo.location: id }))?,
        )?;
        let first = Some("drive-a".to_owned());
        engine.verify_identity_with_drive(&repo, &repo, "password", first.clone())?;
        engine.verify_identity_with_drive(&repo, &repo, "password", first)?;
        assert!(
            engine
                .verify_identity_with_drive(&repo, &repo, "password", Some("drive-b".into()))
                .is_err()
        );
        assert!(
            engine
                .verify_identity_with_drive(&repo, &repo, "password", None)
                .is_err()
        );
        assert_eq!(
            udev_drive_id("E:ID_SERIAL=serial\nE:ID_WWN=wwn\n"),
            Some("wwn".into())
        );
        assert_eq!(udev_drive_id("E:ID_SERIAL=\n"), None);
        Ok(())
    }
}
