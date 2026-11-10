//! Restic CLI boundary. No shell, implicit unlock, or destructive defaults.
mod engine;
mod identity;
mod model;
mod mounts;
mod process;
mod restore;
mod store;

pub use engine::{Capabilities, Engine};
pub use identity::JobLock;
pub use model::{
    Event, JobResult, Outcome, Profile, Repository, Snapshot, SnapshotEntry, classify, snapshot_id,
    valid_id,
};
pub use mounts::{MountInfo, ensure_mounted, mount_info};
pub use store::{
    ProfileList, delete_profile, list_profiles, load_profile, read_profile_file, record,
    save_profile, state_dir,
};

use anyhow::Result;
use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

fn has_parent_traversal(path: &Path) -> bool {
    path.components().any(|part| part == Component::ParentDir)
}
/// Snapshot paths are absolute, NUL-free and never step upwards.
fn valid_snapshot_path(path: &str) -> bool {
    path.starts_with('/') && !path.contains('\0') && !path.split('/').any(|part| part == "..")
}
fn open_directory(path: &Path, extra_flags: i32) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | extra_flags)
        .open(path)?)
}
/// A path that keeps referring to `handle`'s directory even if its original path is replaced.
fn fd_path(handle: &File) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        handle.as_raw_fd()
    ))
}
