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
            ensure!(
                !destination.starts_with(&source) && !source.starts_with(&destination),
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
}
impl Default for Engine {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("/usr/bin/restic"),
            cache_dir: None,
        }
    }
}
impl Engine {
    pub fn version(&self) -> Result<String> {
        let output = Command::new(&self.executable).arg("version").output()?;
        ensure!(output.status.success(), "Restic version detection failed");
        Ok(String::from_utf8(output.stdout)?.trim().into())
    }
    pub fn run(
        &self,
        repo: &Repository,
        password: &str,
        args: &[OsString],
        cancel: Arc<AtomicBool>,
        mut event: impl FnMut(Event),
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
                        if bytes.len() + n <= 1024 * 1024 {
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
                        Err("Process output record exceeded 1 MiB".to_owned())
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
                && self
                    .snapshots(&profile.repository, password)?
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
        self.run(&profile.repository, password, &args, cancel, event)
    }
    pub fn snapshots(&self, repo: &Repository, password: &str) -> Result<Vec<Snapshot>> {
        let mut snapshots = None;
        let result = self.run(
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
        )?;
        ensure!(
            result.outcome == Outcome::Success,
            "Snapshot listing failed: {:?}",
            result.outcome
        );
        snapshots.context("Snapshot response was invalid or exceeded the 1 MiB record limit; listing is unavailable")
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
        let mut result = self.run(repo, password, &args, cancel.clone(), event)?;
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
