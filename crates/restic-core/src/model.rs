use crate::has_parent_traversal;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

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
pub(crate) fn resolved_destination(path: &Path) -> Result<PathBuf> {
    ensure!(!has_parent_traversal(path), "Parent traversal is forbidden");
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
