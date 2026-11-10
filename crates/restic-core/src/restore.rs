use crate::{
    Engine, Event, JobResult, Outcome, Repository, fd_path, identity::PinPolicy, open_directory,
    snapshot_id, valid_snapshot_path,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::CString,
    fs,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Escape Restic include-pattern metacharacters so a filename matches only itself.
fn literal_pattern(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len() + 1);
    escaped.push('/');
    for c in name.chars() {
        if "*?[]\\".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

impl Engine {
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
        let (_handle, opened) = self.open_verified(repo, password, PinPolicy::Record)?;
        ensure!(valid_snapshot_path(selected), "Invalid snapshot path");
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
        let parent_handle = open_directory(&parent, libc::O_NOFOLLOW)?;
        let metadata = parent_handle.metadata()?;
        let euid = unsafe { libc::geteuid() };
        ensure!(
            metadata.uid() == euid && metadata.mode() & 0o022 == 0,
            "Restore parent must be owned by you and not writable by other users (owner {}, effective {euid}, mode {:o})",
            metadata.uid(),
            metadata.mode()
        );
        ensure!(
            parent != Path::new("/")
                && !parent.starts_with("/etc")
                && !parent.starts_with("/usr")
                && !parent.starts_with("/boot"),
            "Unsafe restore location"
        );
        let staging = tempfile::Builder::new()
            .prefix(".restic-studio-restore-")
            .tempdir_in(fd_path(&parent_handle))?;
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
        let args = vec![
            "restore".into(),
            format!("{}:{}", snapshot, selected_parent.display()).into(),
            "--target".into(),
            restore_target.as_os_str().into(),
            "--include".into(),
            literal_pattern(selected_name).into(),
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
        let old = CString::new(
            staging
                .path()
                .file_name()
                .context("Missing staging name")?
                .as_encoded_bytes(),
        )?;
        let new = CString::new(name.as_encoded_bytes())?;
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
            let error = std::io::Error::last_os_error();
            let kept = staging.keep();
            let display_path = parent.join(kept.file_name().context("Missing staging name")?);
            bail!(
                "Could not publish restore: {error}; staged data retained under the destination parent, originally {}",
                display_path.display()
            );
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn include_patterns_match_literally() {
        assert_eq!(super::literal_pattern("a[1]*?.txt"), "/a\\[1\\]\\*\\?.txt");
        assert_eq!(super::literal_pattern("back\\slash"), "/back\\\\slash");
    }
}
