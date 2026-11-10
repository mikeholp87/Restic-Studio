use crate::{JobResult, Profile, valid_id};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Largest profile file accepted; real profiles are a few hundred bytes.
const PROFILE_LIMIT: u64 = 1 << 20;

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
/// A profile JSON file anywhere on disk, read with the same size bound as the store.
pub fn read_profile_file(path: &Path) -> Result<Profile> {
    let file = fs::File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    serde_json::from_reader(file.take(PROFILE_LIMIT))
        .with_context(|| format!("{} is not a valid profile", path.display()))
}
/// Profiles live as `<id>.json` files in `directory`; the filename is the identity.
pub fn load_profile(directory: &Path, id: &str) -> Result<Profile> {
    valid_id(id)?;
    let file = fs::File::open(directory.join(format!("{id}.json")))
        .with_context(|| format!("Profile {id} not found"))?;
    let profile: Profile = serde_json::from_reader(file.take(PROFILE_LIMIT))
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
