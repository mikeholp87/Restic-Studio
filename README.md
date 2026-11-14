# Restic Studio

[![Latest release](https://img.shields.io/github/v/release/mikeholp87/Restic-Studio)](https://github.com/mikeholp87/Restic-Studio/releases/latest) [![License: MIT](https://img.shields.io/github/license/mikeholp87/Restic-Studio)](LICENSE) [![Stars](https://img.shields.io/github/stars/mikeholp87/Restic-Studio?style=flat)](https://github.com/mikeholp87/Restic-Studio/stargazers) ![apt install restic-studio](https://img.shields.io/badge/apt%20install-restic--studio-A81D33?logo=debian&logoColor=white)

Native GTK4 backup and restore manager for Linux, powered by the [Restic](https://restic.net/) CLI. Restic Studio is an **early development preview**, not a verified daily backup or full-system recovery solution.

It provides a simple graphical path from a local folder to an encrypted Restic repository, including repositories on mounted external drives. The repository password is entered for each operation and is not saved by the app.

## What works today

| Area | Current behavior |
| --- | --- |
| Repositories | Initialize a new, empty local repository or open an existing one. Choose a mounted external-drive folder; the picker recognizes a repository in that folder or its single immediate child. |
| Backup | Select one source folder, preview with Restic `backup --dry-run`, or create a snapshot with **Back Up Now**. A preview does not upload data or create a snapshot. With a saved profile loaded and its repository and first source unchanged, backup and preview use all of the profile's sources, exclusions and options; editing either field makes it a one-off manual backup. GUI backups are recorded in the same activity history as the runner's. |
| Progress | Backup, preview, and restore show a popup with Restic's reported file and byte counts. The bar shows a percentage only when Restic reports a total. |
| Stop | **Stop safely** requests a graceful interrupt and waits for Restic to exit. A stopped backup may still have created a snapshot; refresh the list before retrying. |
| Snapshots | List snapshots and show up to the first 500 entries returned for a selected directory. |
| Restore | Restore one selected file or directory into a **new** destination folder. Restic verifies the restored data; the app stages it privately and publishes it without replacing an existing destination. A stopped restore does not publish the staged files. |
| Checks | Run Restic's repository structure check, with the same job lock and repository identity checks as a backup. A successful backup alone is not a health check. |
| Runner | Use the separate `restic-studio` CLI for the same core operations and JSON profile files. It records headless backup activity under XDG state. |
| Appearance | System, light, and dark theme selection in the GTK interface. |

This release supports **local filesystem repositories only**. An external drive must already be mounted; Restic Studio does not mount, format, or repair it.

## Install on Debian

The development package targets Debian forky/sid (it depends on the system Restic 0.19, GTK 4.24, libadwaita 1.10 and glibc 2.43). It has not been verified on Debian 13, KDE, or X11.

Install from the apt repository:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://mikeholp87.github.io/apt/apt.asc | sudo tee /etc/apt/keyrings/mikeholp87-apt.asc >/dev/null
echo "deb [signed-by=/etc/apt/keyrings/mikeholp87-apt.asc] https://mikeholp87.github.io/apt/ ./" | sudo tee /etc/apt/sources.list.d/mikeholp87-apt.list
sudo apt update && sudo apt install restic-studio
```

Or build it from this checkout:

```sh
cargo build --workspace --release
sh packaging/debian/build.sh
sudo apt install ./dist/restic-studio_*_amd64.deb
```

Launch **Restic Studio** from the application menu or run `restic-studio-gui`. Ordinary use does not require `sudo`. See [installation and uninstall details](docs/installation.md).

For local development, install `build-essential`, `pkg-config`, `libgtk-4-dev`, `libadwaita-1-dev`, and `restic`, then run:

```sh
cargo build
cargo test
cargo run -p restic-studio-gui
```

The GUI is a workspace member outside `default-members`, so plain `cargo build` and `cargo test` cover the core and runner without GTK development libraries; add `--workspace` to include the GUI. The tested environment used Rust 1.96.0, Restic 0.19.1, GTK 4.24.1, and libadwaita 1.10.0. The package currently declares Restic 0.19 or newer and the native library versions used for this build; other combinations require validation.

## First backup

1. Choose a folder on a mounted external drive. Select the repository directory itself when several repositories are present. For a new destination, the picker proposes a `restic-studio` child directory.
2. For a new repository, enter a strong password and choose **Initialize empty repository**. For an existing repository, enter its current password. Losing that password can make recovery impossible.
3. Select a source folder. For a personal backup, choose your own home directory (for example `/home/your-user`). Selecting all of `/home` may include files your account cannot read.
4. Choose **Preview backup (no snapshot)**. The preview checks current access and options but does not prove that a later backup or restore will succeed.
5. Choose **Back Up Now** and watch the progress popup. Check the final outcome. Restic exit code 3 is reported as a partial backup, which may have created an incomplete snapshot.
6. Refresh snapshots and run **Check repository**. Practice restoring a file to a new folder before relying on the backup.

The app refuses to back up if the selected repository is missing and prevents recursive source/repository layouts; a `/` backup is allowed because it explicitly excludes the repository. Backup, restore and check remember the repository ID and, when available, the drive's hardware identifier. If a previously identified drive changes or its identifier is unavailable, the operation stops. Listing and browsing snapshots compare against a remembered identity but never record one. Some drives do not expose an identifier, and cloned identifiers are possible. The GUI also checks the selected destination's device before each operation. The app does not silently create a replacement repository when a drive is unavailable.

When the source is `/`, Restic Studio automatically excludes the selected repository folder, plus `/dev`, `/proc`, `/sys`, `/run`, `/tmp`, `/var/tmp`, `/mnt`, `/media`, and `/lost+found`. It also excludes `/home` when the selected repository already contains a snapshot of `/home` or a subdirectory of it from this host. The app still runs as your user, so protected files outside those paths can cause a partial backup; it will report that result instead of marking it successful.

## Restore a file or directory

1. Open the **Restore** tab, enter the repository password, and refresh snapshots.
2. Select a snapshot, browse its directory, and select one file or folder.
3. Enter an absolute path for a **new** destination folder beneath a private directory owned by you, such as `/home/your-user/restored-file`. The parent must exist and must not be writable by other users.
4. Start the restore and review its popup and final result. The app keeps the original snapshot path beneath the new destination folder.

Existing destinations are never overwritten. Restores under `/`, `/etc`, `/usr`, or `/boot` are blocked. The app does not restore over a running system or change disk partitions.

## Headless runner

`restic-studio` supports:

```text
version
init REPO
snapshots REPO
ls REPO FULL_SNAPSHOT_ID DIRECTORY
check REPO
preview PROFILE.json
backup PROFILE.json
run-profile ID
restore REPO FULL_SNAPSHOT_ID SNAPSHOT_PATH NEW_DIRECTORY
```

Repository and source paths must be absolute. A profile file has this structure:

```json
{
  "id": "personal",
  "name": "Personal files",
  "repository": { "location": "/path/to/mounted-drive/ResticRepo" },
  "sources": ["/home/your-user"],
  "excludes": [],
  "one_file_system": false
}
```

The runner reads the password from standard input through a protected pipe. Do not put it in command arguments, environment variables, shell history, or a profile file. `run-profile ID` reads `$XDG_STATE_HOME/restic-studio/profiles/ID.json` (usually `~/.local/state/restic-studio/profiles/ID.json`). It requires credentials on stdin and is **not** an unattended scheduler. Backup jobs use a local lock and append activity to XDG state; preview does not record a backup job.

## Security and recovery boundaries

- The GUI never runs as root. Both source files and repository files must be readable by the current user. A repository with root-owned, owner-only files will fail to open until its ownership or permissions are repaired outside the app.
- Restic performs encryption, backup, and verification. The app invokes its documented CLI with structured arguments, not shell command strings, and does not store the repository password.
- Backup outcomes distinguish success, partial completion, interruption, lock conflicts, missing repositories, and wrong passwords. A backup result is not proof that stored data can be restored.
- A restore writes into a private staging directory, verifies via Restic, and publishes only to a new path. A failed or cancelled restore does not overwrite the requested destination.
- No automatic repository unlock, snapshot deletion, retention, pruning, system-file overwrite, ownership repair, mount, or partition change is performed.

See the [architecture](docs/architecture.md) and [verification report](docs/verification.md) for implementation details and tested limits.

## What is not implemented yet

There is no graphical multi-profile editor or setup wizard; no scheduled unattended backup, keyring integration, remote repository configuration, retention/pruning UI, snapshot search or comparison, in-place restore, full-home migration workflow, or privileged system backup. Large snapshot listings are bounded, and the GUI shows at most 500 entries per directory. See the [remaining work](docs/remaining-work.md).

Automated recovery tests use disposable repositories and synthetic files. Core and GUI tests, release builds, formatting, Clippy, shell lint, and a secret scan passed in the development environment. The GTK window was not visually tested from the sandbox, and recovery from the user's external repository remains unverified. No performance benchmarks or KDE/X11 compatibility results are claimed.

## Project layout

```text
crates/restic-core/      Restic subprocesses, validation, restore safety, profiles
crates/restic-runner/    Headless CLI and job history
crates/restic-gui/       GTK4/libadwaita interface
packaging/debian/        Development .deb builder
packaging/desktop/       Desktop entry and icon
docs/                   Architecture, installation, verification, roadmap
```

The project uses the [MIT license](LICENSE).
