use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize)]
pub struct MountInfo {
    pub mount_point: PathBuf,
    pub fstype: String,
    pub source: String,
    /// Kernel's removable flag for the underlying disk; None when sysfs does not say.
    pub removable: Option<bool>,
}
/// Undo mountinfo's octal escapes (`\040` space, `\011` tab, `\012` newline, `\134` backslash).
fn unescape_mount_path(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && let Some(code) = bytes.get(i + 1..i + 4)
            && code.iter().all(|b| (b'0'..=b'7').contains(b))
            && let Some(byte) = std::str::from_utf8(code)
                .ok()
                .and_then(|digits| u8::from_str_radix(digits, 8).ok())
        {
            out.push(byte);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    PathBuf::from(OsString::from_vec(out))
}
/// One mountinfo line as (mount point, major:minor, fstype, source); None when malformed.
fn parse_mount_line(line: &str) -> Option<(PathBuf, &str, &str, &str)> {
    let (head, tail) = line.split_once(" - ")?;
    let fields: Vec<&str> = head.split(' ').collect();
    let mut after = tail.split(' ');
    let (fstype, source) = (after.next()?, after.next()?);
    Some((
        unescape_mount_path(fields.get(4)?),
        fields.get(2)?,
        fstype,
        source,
    ))
}
/// Longest mount point containing `path`, from a /proc/self/mountinfo table.
fn parse_mount(table: &str, path: &Path) -> Option<MountInfo> {
    let mut best: Option<(MountInfo, String)> = None;
    for (mount_point, majmin, fstype, source) in table.lines().filter_map(parse_mount_line) {
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
            best = Some((info, majmin.to_owned()));
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

pub(crate) fn udev_drive_id(data: &str) -> Option<String> {
    ["E:ID_WWN_WITH_EXTENSION=", "E:ID_WWN=", "E:ID_SERIAL="]
        .iter()
        .find_map(|prefix| {
            data.lines()
                .find_map(|line| line.strip_prefix(prefix).filter(|value| !value.is_empty()))
        })
        .map(str::to_owned)
}
/// Hardware identifier of the block device holding `path`, when udev records one.
pub(crate) fn drive_id(path: &Path) -> Result<Option<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn malformed_lines_are_skipped_and_escapes_decoded() {
        let table = "garbage line without separator\n\
            22 1 259:2 / / rw - ext4 /dev/nvme0n1p2 rw\n\
            91 22 8:33 / /media/a\\134b\\012c rw - vfat /dev/sdc1 rw\n";
        let drive = parse_mount(table, Path::new("/media/a\\b\nc/repo")).unwrap();
        assert_eq!(drive.mount_point, Path::new("/media/a\\b\nc"));
        assert_eq!(
            parse_mount(table, Path::new("/home")).unwrap().mount_point,
            Path::new("/")
        );
        assert_eq!(unescape_mount_path("x\\999y"), Path::new("x\\999y"));
        assert_eq!(
            udev_drive_id("E:ID_SERIAL=serial\nE:ID_WWN=wwn\n"),
            Some("wwn".into())
        );
        assert_eq!(udev_drive_id("E:ID_SERIAL=\n"), None);
    }
}
