use crate::{
    Engine, Event, JobResult, Repository, classify, fd_path, has_parent_traversal, open_directory,
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Largest single line of Restic output accepted by `run`.
pub(crate) const RECORD_LIMIT: usize = 1 << 20;
/// Secrets shorter than this are only redacted as whole words, so a one-letter
/// password does not erase that letter from every message.
const WHOLE_WORD_SECRET: usize = 8;
const MAX_ERRORS: usize = 8;
const MAX_MESSAGE_CHARS: usize = 2048;

fn redact_secret(text: &str, secret: &str) -> String {
    if secret.len() >= WHOLE_WORD_SECRET {
        return text.replace(secret, "[REDACTED]");
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, _) in text.match_indices(secret) {
        let end = start + secret.len();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
        if boundary(text[..start].chars().next_back()) && boundary(text[end..].chars().next()) {
            out.push_str(&text[last..start]);
            out.push_str("[REDACTED]");
            last = end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Removes the password and every spelling of the repository path from Restic output.
struct Redactor<'a> {
    password: &'a str,
    locations: Vec<String>,
}
impl<'a> Redactor<'a> {
    fn new(password: &'a str, repo: &Repository) -> Self {
        let mut locations = vec![repo.location.clone()];
        // A pinned `/proc/<pid>/fd/<n>` location also names the real directory behind it.
        if let Ok(real) = fs::read_link(&repo.location) {
            locations.push(real.to_string_lossy().into_owned());
        }
        locations.retain(|location| !location.is_empty());
        // Longest first, so a path is not partly replaced by its own prefix.
        locations.sort_by_key(|location| std::cmp::Reverse(location.len()));
        Self {
            password,
            locations,
        }
    }
    fn apply(&self, text: &str) -> String {
        self.locations
            .iter()
            .fold(redact_secret(text, self.password), |text, location| {
                text.replace(location, "[repository]")
            })
    }
}

/// Reads one Restic output stream into bounded records. Oversized records are drained.
fn spawn_reader(
    stream: Box<dyn std::io::Read + Send>,
    tx: mpsc::SyncSender<std::result::Result<Value, String>>,
    max_record: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let mut bytes = Vec::new();
            let mut oversized = false;
            loop {
                let buffer = match reader.fill_buf() {
                    Ok(b) => b,
                    Err(error) => {
                        let _ = tx.send(Err(format!("Process output read failed: {error}")));
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
                    String::from_utf8_lossy(&bytes)
                        .chars()
                        .take(MAX_MESSAGE_CHARS)
                        .collect()
                })
            };
            if tx.send(value).is_err() {
                return;
            }
        }
    })
}

fn error_message(value: &Value) -> Option<&str> {
    if value["message_type"] != "exit_error" && value["message_type"] != "error" {
        return None;
    }
    value.get("message").and_then(Value::as_str).or_else(|| {
        value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
    })
}

impl Engine {
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
        let location = Path::new(&repo.location);
        ensure!(
            location.is_absolute(),
            "Only local absolute repositories are supported"
        );
        ensure!(
            !has_parent_traversal(location),
            "Repository path may not contain parent traversal"
        );
        let (pinned, suffix) = if location.exists() {
            (open_directory(location, 0)?, None)
        } else {
            let parent = location.parent().context("Repository parent missing")?;
            (
                open_directory(parent, 0)?,
                Some(location.file_name().context("Repository name missing")?),
            )
        };
        let mut pinned_location = fd_path(&pinned);
        if let Some(name) = suffix {
            pinned_location.push(name);
        }
        let redactor = Redactor::new(password, repo);
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
        let out = spawn_reader(Box::new(stdout), tx.clone(), max_record);
        let err = spawn_reader(Box::new(stderr), tx, max_record);
        let mut snapshot = None;
        let mut errors = Vec::new();
        let mut interrupted = false;
        loop {
            if cancel.load(Ordering::Relaxed) && !interrupted {
                // SIGINT asks Restic to finish cleanup; forced termination is deliberately not automatic.
                if let Ok(pid) = libc::pid_t::try_from(child.id()) {
                    unsafe {
                        libc::kill(pid, libc::SIGINT);
                    }
                }
                interrupted = true;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(value)) => {
                    if let Some(message) = error_message(&value)
                        && errors.len() < MAX_ERRORS
                    {
                        let message: String = message.chars().take(MAX_MESSAGE_CHARS).collect();
                        errors.push(redactor.apply(&message));
                    }
                    if let Some(id) = value.get("snapshot_id").and_then(Value::as_str) {
                        snapshot = Some(id.into());
                    }
                    event(Event::Json(value));
                }
                Ok(Err(message)) => {
                    let message = redactor.apply(&message);
                    if !message.trim().is_empty() && errors.len() < MAX_ERRORS {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_secrets_are_redacted_as_whole_words_only() {
        assert_eq!(
            redact_secret("Fatal: wrong password a", "a"),
            "Fatal: wrong password [REDACTED]"
        );
        assert_eq!(redact_secret("abab ab", "ab"), "abab [REDACTED]");
        assert_eq!(
            redact_secret("xcorrect-horse-batteryx", "correct-horse-battery"),
            "x[REDACTED]x"
        );
    }

    #[test]
    fn pinned_location_redacts_the_real_path() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let handle = open_directory(temp.path(), 0)?;
        let pinned = Repository {
            location: fd_path(&handle).to_string_lossy().into_owned(),
        };
        let real = temp.path().canonicalize()?;
        let text = format!("cannot open {}/config", real.display());
        assert_eq!(
            Redactor::new("password1", &pinned).apply(&text),
            "cannot open [repository]/config"
        );
        Ok(())
    }
}
